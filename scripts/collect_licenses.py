#!/usr/bin/env python3
"""Collect installed dependency license texts without networking or invoking Cargo.

Python 3.11+ uses tomllib; Python 3.10 requires the locally installed tomli package.
The Cargo collection is deliberately a Cargo.lock superset, not a link-time SBOM.
Only npm production dependency closures are selected. Unresolved files/metadata
are reported rather than replaced with inferred licenses or downloaded material.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import sys
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:
    try:
        import tomli as tomllib
    except ModuleNotFoundError:
        tomllib = None  # type: ignore[assignment]


LICENSE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE")
PRUNED_DIRECTORIES = {".git", "node_modules", "target", "__pycache__"}
MAX_TEXT_BYTES = 16 * 1024 * 1024


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def file_digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8-sig"))
    if not isinstance(value, dict):
        raise ValueError(f"Expected a JSON object: {path}")
    return value


def read_toml(path: Path) -> dict[str, Any]:
    if tomllib is None:
        raise RuntimeError("Python 3.11+ or a locally installed tomli is required; no dependency is downloaded.")
    with path.open("rb") as stream:
        return tomllib.load(stream)


def parse_toml_snapshot(content: bytes) -> dict[str, Any]:
    if tomllib is None:
        raise RuntimeError("Python 3.11+ or a locally installed tomli is required; no dependency is downloaded.")
    return tomllib.loads(content.decode("utf-8-sig"))


def within(path: Path, root: Path) -> bool:
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except (OSError, ValueError):
        return False


def safe_component(value: str) -> str:
    value = re.sub(r"[^A-Za-z0-9._-]+", "_", value).strip(".")
    return value or "unnamed"


def issue(record: dict[str, Any], code: str, detail: str) -> None:
    record["issues"].append({"code": code, "detail": detail})


def new_record(ecosystem: str, identity: str, name: str, version: str) -> dict[str, Any]:
    return {
        "ecosystem": ecosystem,
        "id": identity,
        "name": name,
        "version": version,
        "license_expression": None,
        "repository": None,
        "license_files": [],
        "issues": [],
    }


def atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".collect-licenses.tmp")
    if temporary.is_symlink():
        raise ValueError(f"Refusing a symlink temporary output: {temporary}")
    temporary.write_bytes(data)
    os.replace(temporary, path)


def license_candidates(root: Path, declared: str | None, record: dict[str, Any]) -> list[Path]:
    found: set[Path] = set()
    for current, directories, files in os.walk(root, followlinks=False):
        current_path = Path(current)
        directories[:] = sorted(
            name for name in directories
            if name not in PRUNED_DIRECTORIES
            and not (current_path / name).is_symlink()
            and within(current_path / name, root)
        )
        in_license_directory = any(
            part.upper().startswith(LICENSE_PREFIXES)
            for part in current_path.relative_to(root).parts
        )
        for name in sorted(files):
            if not (in_license_directory or name.upper().startswith(LICENSE_PREFIXES)):
                continue
            path = current_path / name
            if path.is_symlink() or not within(path, root):
                issue(record, "unsafe_license_path", str(path.relative_to(root)))
            elif path.is_file():
                found.add(path)
    if declared:
        candidate = root / declared
        if candidate.is_symlink() or not within(candidate, root):
            issue(record, "unsafe_declared_license_file", declared)
        elif candidate.is_file():
            found.add(candidate)
        else:
            issue(record, "declared_license_file_missing", declared)
    return sorted(found, key=lambda item: item.relative_to(root).as_posix())


def copy_licenses(root: Path, declared: str | None, record: dict[str, Any], output: Path) -> None:
    relative_base = Path("licenses") / record["ecosystem"] / (
        safe_component(record["name"]) + "-" + safe_component(record["version"])
        + "-" + digest(record["id"].encode("utf-8"))[:10]
    )
    for source in license_candidates(root, declared, record):
        relative_source = source.relative_to(root)
        destination = output / relative_base / relative_source
        if destination.is_symlink() or not within(destination, output):
            issue(record, "unsafe_output_path", str(destination))
            continue
        try:
            if source.stat().st_size > MAX_TEXT_BYTES:
                issue(record, "license_file_exceeds_size_limit", relative_source.as_posix())
                continue
            content = source.read_bytes()
            atomic_write(destination, content)
            is_spdx_metadata = content.lstrip().startswith(b"SPDXVersion:") and b"ExtractedText:" not in content
            record["license_files"].append({
                "source_relative_path": relative_source.as_posix(),
                "output_relative_path": destination.relative_to(output).as_posix(),
                "bytes": len(content),
                "sha256": digest(content),
                "kind": "spdx_metadata" if is_spdx_metadata else "license_or_notice_file",
            })
            if not content.strip():
                issue(record, "empty_license_file", relative_source.as_posix())
        except OSError as exc:
            issue(record, "license_file_io_error", f"{relative_source.as_posix()}: {exc}")
    if not record["license_files"]:
        issue(record, "license_text_missing", "No installed LICENSE*/LICENCE*/COPYING*/NOTICE* or declared license-file was collected.")
    elif all(item["kind"] == "spdx_metadata" for item in record["license_files"]):
        issue(record, "license_text_missing", "Only SPDX metadata without ExtractedText is installed; full license text has not been substituted from another package.")


def collect_cargo(project: Path, cargo_home: Path, output: Path) -> dict[str, Any]:
    lock_path = project / "Cargo.lock"
    lock_bytes = lock_path.read_bytes()
    lock = parse_toml_snapshot(lock_bytes)
    records: list[dict[str, Any]] = []
    skipped: list[dict[str, str]] = []
    registry_root = cargo_home / "registry" / "src"
    registries = sorted(path for path in registry_root.glob("*") if path.is_dir())
    for package in sorted(lock.get("package", []), key=lambda item: (item.get("name", ""), item.get("version", ""), item.get("source", ""))):
        name, version = str(package["name"]), str(package["version"])
        source = package.get("source")
        if not source:
            skipped.append({"name": name, "version": version, "reason": "workspace_or_path_package_not_collected"})
            continue
        record = new_record("cargo", f"cargo:{name}@{version}#{source}", name, version)
        record["lock_source"] = source
        record["lock_checksum"] = package.get("checksum")
        records.append(record)
        if not isinstance(source, str) or not source.startswith("registry+"):
            issue(record, "unsupported_offline_source", "Only installed Cargo registry sources are collected; no git/network checkout is performed.")
            continue
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", name) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", version):
            issue(record, "unsafe_package_identity", "Package name or version is not a safe registry directory component.")
            continue
        verified: list[tuple[Path, dict[str, Any], str]] = []
        candidate_problems: list[str] = []
        for registry in registries:
            candidate = registry / f"{name}-{version}"
            if not candidate.is_dir() or not within(candidate, registry_root):
                continue
            try:
                metadata = read_toml(candidate / "Cargo.toml").get("package", {})
                if metadata.get("name") != name or metadata.get("version") != version:
                    candidate_problems.append(f"{registry.name}: package metadata name/version mismatch")
                    continue
                expected = package.get("checksum")
                checksum_path = candidate / ".cargo-checksum.json"
                archive = cargo_home / "registry" / "cache" / registry.name / f"{name}-{version}.crate"
                validation = "Cargo.toml name/version match Cargo.lock; archive checksum unavailable locally, so source identity is metadata-only."
                if expected and checksum_path.is_file():
                    if read_json(checksum_path).get("package") != expected:
                        candidate_problems.append(f"{registry.name}: mismatched .cargo-checksum.json package value")
                        continue
                    validation = "Cargo.toml name/version and .cargo-checksum.json package value match Cargo.lock; individual extracted files were not rehashed."
                elif expected and archive.is_file() and within(archive, cargo_home / "registry" / "cache"):
                    if file_digest(archive) != expected:
                        candidate_problems.append(f"{registry.name}: cached .crate SHA-256 does not match Cargo.lock")
                        continue
                    validation = "Cargo.toml name/version match Cargo.lock and cached .crate SHA-256 matches the lock checksum; individual extracted files were not compared to the archive."
                verified.append((candidate, metadata, validation))
            except (OSError, ValueError) as exc:
                candidate_problems.append(f"{registry.name}: {type(exc).__name__}: {exc}")
        if not verified:
            issue(record, "registry_source_missing_or_unverified", "; ".join(candidate_problems) or "Exact name/version registry source was not found locally.")
            continue
        # Prefer a checksum-correlated source to a metadata-only candidate.
        verified.sort(key=lambda item: ("metadata-only" in item[2], item[0].as_posix()))
        if len(verified) > 1 and "metadata-only" in verified[0][2]:
            issue(record, "ambiguous_registry_source", "Multiple metadata-only sources exist for this name/version; no source was chosen.")
            continue
        selected, metadata, validation = verified[0]
        record["registry_source_relative_path"] = selected.relative_to(registry_root).as_posix()
        record["source_validation"] = validation
        if len(verified) > 1:
            record["matching_metadata_candidates"] = [path.relative_to(registry_root).as_posix() for path, _, _ in verified]
        license_value = metadata.get("license")
        record["license_expression"] = license_value if isinstance(license_value, str) else None
        record["repository"] = metadata.get("repository")
        declared = metadata.get("license-file", metadata.get("license_file"))
        record["declared_license_file"] = declared
        if not record["license_expression"] and not isinstance(declared, str):
            issue(record, "license_metadata_missing", "No string package.license or package.license-file in the installed Cargo.toml.")
        copy_licenses(selected, declared if isinstance(declared, str) else None, record, output)
    return {
        "scope": "All external Cargo.lock packages: target/optional/build/dev dependency superset, NOT proof that every collected package is linked into this Windows release.",
        "superset": True,
        "lockfile_sha256": digest(lock_bytes),
        "packages": records,
        "skipped_packages": skipped,
    }


def npm_resolve(packages: dict[str, Any], owner: str, name: str) -> str | None:
    current = owner
    while True:
        candidate = (current + "/" if current else "") + "node_modules/" + name
        if candidate in packages:
            return candidate
        if not current:
            return None
        current = current.rsplit("/node_modules/", 1)[0] if "/node_modules/" in current else ""


def collect_npm(project: Path, output: Path) -> dict[str, Any]:
    lock_path = project / "package-lock.json"
    lock_bytes = lock_path.read_bytes()
    lock = json.loads(lock_bytes.decode("utf-8-sig"))
    if not isinstance(lock, dict):
        raise ValueError("package-lock.json must contain a JSON object.")
    packages = lock.get("packages")
    if not isinstance(packages, dict) or "" not in packages:
        raise ValueError("package-lock.json v2/v3 with a packages table is required.")
    root = packages[""]
    requests: list[tuple[str, str, bool, str]] = []
    for name in sorted(root.get("dependencies", {})):
        requests.append(("", name, False, "root production dependency"))
    for name in sorted(root.get("optionalDependencies", {})):
        requests.append(("", name, True, "root optional production dependency"))
    selected: dict[str, dict[str, Any]] = {}
    unresolved: list[dict[str, Any]] = []
    while requests:
        owner, name, optional, reason = requests.pop(0)
        key = npm_resolve(packages, owner, name)
        if key is None:
            unresolved.append({"owner": owner or "<root>", "name": name, "optional": optional, "reason": "Not resolved in the lockfile packages table"})
            continue
        if key in selected:
            became_required = selected[key]["optional"] and not optional
            selected[key]["optional"] = selected[key]["optional"] and optional
            selected[key]["required_by"].append({"owner": owner or "<root>", "reason": reason})
            if not became_required:
                continue
        else:
            selected[key] = {"optional": optional, "required_by": [{"owner": owner or "<root>", "reason": reason}]}
        entry = packages[key]
        optional_dependencies = entry.get("optionalDependencies", {})
        for dependency in sorted(set(entry.get("dependencies", {})) | set(optional_dependencies)):
            requests.append((key, dependency, optional or dependency in optional_dependencies, "dependency closure"))
        peer_metadata = entry.get("peerDependenciesMeta", {})
        for dependency in sorted(entry.get("peerDependencies", {})):
            peer_optional = bool(peer_metadata.get(dependency, {}).get("optional"))
            requests.append((key, dependency, optional or peer_optional, "peer dependency closure"))
    records: list[dict[str, Any]] = []
    skipped: list[dict[str, Any]] = []
    for key, selection in sorted(selected.items()):
        entry = packages[key]
        # Lock keys identify installed locations; scoped package names stay intact.
        inferred_name = key.rsplit("node_modules/", 1)[-1]
        name = str(entry.get("name", inferred_name))
        version = str(entry.get("version", "unknown"))
        record = new_record("npm", f"npm:{key}@{version}", name, version)
        record["lock_package_path"] = key
        record["lock_integrity"] = entry.get("integrity")
        record["lock_license_expression"] = entry.get("license")
        record["required_by"] = selection["required_by"]
        record["optional"] = selection["optional"]
        parts = PurePosixPath(key).parts
        source = project / Path(*parts)
        if PurePosixPath(key).is_absolute() or ".." in parts or "\\" in key or not within(source, project / "node_modules"):
            records.append(record)
            issue(record, "unsafe_npm_package_path", key)
            continue
        if not source.is_dir() and selection["optional"]:
            skipped.append({"name": name, "version": version, "lock_package_path": key, "reason": "optional_production_dependency_not_installed"})
            continue
        records.append(record)
        try:
            metadata = read_json(source / "package.json")
            if metadata.get("name") != name or metadata.get("version") != version:
                issue(record, "installed_metadata_mismatch", "Installed package.json name/version does not match package-lock.json; files were not copied.")
                continue
        except (OSError, ValueError) as exc:
            issue(record, "npm_source_missing_or_invalid", f"{type(exc).__name__}: {exc}")
            continue
        record["source_validation"] = "Installed package.json name/version match package-lock.json; tarball integrity was not independently reconstructed."
        record["license_expression"] = metadata.get("license")
        record["repository"] = metadata.get("repository")
        if not record["license_expression"]:
            issue(record, "license_metadata_missing", "Installed package.json has no license field; lockfile metadata is retained separately, not substituted.")
        copy_licenses(source, None, record, output)
    return {
        "scope": "Installed closure of package-lock root dependencies and optionalDependencies, including their resolved peers; root devDependencies are excluded.",
        "superset": False,
        "lockfile_sha256": digest(lock_bytes),
        "packages": records,
        "skipped_packages": skipped,
        "unresolved_dependencies": unresolved,
    }


def apply_overrides(records: list[dict[str, Any]], index_path: Path, output: Path) -> dict[str, Any]:
    if not index_path.is_file():
        return {"available": False, "applied_packages": 0}
    index_bytes = index_path.read_bytes()
    index = json.loads(index_bytes.decode("utf-8-sig"))
    if index.get("schema_version") != 1 or not isinstance(index.get("packages"), list):
        raise ValueError("Unsupported license override index schema.")
    entries: dict[tuple[str, str, str], dict[str, Any]] = {}
    for entry in index["packages"]:
        key = (entry["ecosystem"], entry["name"], entry["version"])
        if key in entries:
            raise ValueError(f"Duplicate license override entry: {key}")
        entries[key] = entry
    applied = 0
    matched: set[tuple[str, str, str]] = set()
    for record in records:
        key = (record["ecosystem"], record["name"], record["version"])
        if key not in entries:
            continue
        matched.add(key)
        entry = entries[key]
        if not record.get("source_validation") or entry.get("license_expression") != record.get("license_expression"):
            issue(record, "override_metadata_mismatch", "Supplement requires verified installed name/version and an exact license-expression match.")
            continue
        if record["ecosystem"] == "cargo":
            matches_lock = bool(entry.get("expected_lock_checksum")) and entry.get("expected_lock_source") == record.get("lock_source") and entry.get("expected_lock_checksum") == record.get("lock_checksum")
        else:
            matches_lock = bool(entry.get("expected_lock_integrity")) and entry.get("expected_lock_integrity") == record.get("lock_integrity")
        if not matches_lock:
            issue(record, "override_lock_mismatch", "Supplemented package checksum/source or npm integrity differs from this lockfile.")
            continue
        relative_base = Path("licenses") / record["ecosystem"] / (
            safe_component(record["name"]) + "-" + safe_component(record["version"])
            + "-" + digest(record["id"].encode("utf-8"))[:10]
        ) / "upstream"
        added = False
        for file in entry.get("files", []):
            relative_source = PurePosixPath(file["path"])
            source = index_path.parent / Path(*relative_source.parts)
            filename = file["filename"]
            destination = output / relative_base / filename
            if relative_source.is_absolute() or ".." in relative_source.parts or "\\" in file["path"] or not within(source, index_path.parent) or source.is_symlink():
                issue(record, "unsafe_override_path", file["path"])
                continue
            if not filename or re.search(r"[/\\]", filename) or filename in {".", ".."} or not within(destination, output) or destination.is_symlink():
                issue(record, "unsafe_override_output", filename)
                continue
            if not isinstance(file.get("source_url"), str) or not file["source_url"].startswith("https://"):
                issue(record, "override_provenance_missing", filename)
                continue
            try:
                if source.stat().st_size > MAX_TEXT_BYTES:
                    issue(record, "license_file_exceeds_size_limit", file["path"])
                    continue
                content = source.read_bytes()
                if not content.strip() or digest(content) != file.get("sha256"):
                    issue(record, "override_hash_mismatch_or_empty", file["path"])
                    continue
                atomic_write(destination, content)
                metadata_only = content.lstrip().startswith(b"SPDXVersion:") and b"ExtractedText:" not in content
                record["license_files"].append({
                    "source_relative_path": file["path"],
                    "output_relative_path": destination.relative_to(output).as_posix(),
                    "bytes": len(content),
                    "sha256": digest(content),
                    "kind": "spdx_metadata" if metadata_only else "license_or_notice_file",
                    "origin": "exact_version_official_upstream_override",
                    "source_url": file["source_url"],
                    "revision": file.get("revision"),
                    "repository_path": file.get("repository_path"),
                })
                added = added or not metadata_only
            except OSError as exc:
                issue(record, "override_file_io_error", f"{file['path']}: {exc}")
        if added:
            applied += 1
            record["issues"] = [item for item in record["issues"] if item["code"] != "license_text_missing"]
            record["override_provenance"] = {
                "revision": entry.get("revision"),
                "repository": entry.get("repository"),
                "evidence": entry.get("evidence"),
                "retrieved_at_utc": index.get("retrieved_at_utc"),
            }
    return {
        "available": True,
        "index_sha256": digest(index_bytes),
        "applied_packages": applied,
        "retrieved_at_utc": index.get("retrieved_at_utc"),
        "unused_entries": [dict(ecosystem=key[0], name=key[1], version=key[2]) for key in sorted(set(entries) - matched)],
        "note": "Previously retrieved exact-version upstream license texts; local hashes and lock identities are checked, and no URL is fetched by this collector.",
    }


def inspect_build_inputs(path: Path | None) -> tuple[dict[str, Any], set[str]]:
    if path is None:
        return {"available": False, "note": "No --build-inputs directory supplied; lockfile superset has not been narrowed."}, set()
    if not path.is_dir():
        raise ValueError(f"Build-input evidence directory does not exist: {path}")
    observed: set[str] = set()
    files: list[dict[str, Any]] = []
    for dependency_file in sorted(path.rglob("*.d")):
        if dependency_file.is_symlink() or not within(dependency_file, path):
            continue
        content = dependency_file.read_bytes()
        matches = re.findall(r"[/\\]registry[/\\]src[/\\][^/\\\s]+[/\\]([^/\\\s]+)", content.decode("utf-8", errors="replace"))
        observed.update(matches)
        files.append({"relative_path": dependency_file.relative_to(path).as_posix(), "sha256": digest(content)})
    return {
        "available": bool(files),
        "directory": str(path.resolve()),
        "dependency_file_count": len(files),
        "unique_registry_source_directories": len(observed),
        "files": files,
        "note": "Observed rustc .d build inputs are a build/host/proc-macro superset, not a link-time SBOM. Absence here alone is not proof that a crate is not linked. Run after the relevant build completes.",
    }, observed


def collect_mpl_sources(records: list[dict[str, Any]], cargo_home: Path, output: Path) -> dict[str, Any]:
    """Keep exact cached source archives for observed MPL packages, outside own code."""
    sources: list[dict[str, Any]] = []
    for record in records:
        if not record.get("observed_in_build_inputs") or "MPL-" not in (record.get("license_expression") or ""):
            continue
        registry_relative = record.get("registry_source_relative_path")
        if not registry_relative:
            issue(record, "mpl_source_archive_missing", "No verified installed registry source identifies the corresponding cached archive.")
            continue
        registry_name = PurePosixPath(registry_relative).parts[0]
        filename = record["name"] + "-" + record["version"] + ".crate"
        source = cargo_home / "registry" / "cache" / registry_name / filename
        destination = output / "licenses" / "third-party-sources" / filename
        try:
            if source.is_symlink() or not within(source, cargo_home / "registry" / "cache") or destination.is_symlink() or not within(destination, output):
                issue(record, "unsafe_mpl_source_archive_path", filename)
                continue
            content = source.read_bytes()
            actual_hash = digest(content)
            if actual_hash != record.get("lock_checksum"):
                issue(record, "mpl_source_archive_checksum_mismatch", filename)
                continue
            atomic_write(destination, content)
            crates_io = record.get("lock_source") in {
                "registry+https://github.com/rust-lang/crates.io-index",
                "sparse+https://index.crates.io/",
                "registry+sparse+https://index.crates.io/",
            }
            sources.append({
                "name": record["name"],
                "version": record["version"],
                "license_expression": record["license_expression"],
                "repository": record.get("repository"),
                "lock_source": record["lock_source"],
                "lock_checksum": record["lock_checksum"],
                "sha256": actual_hash,
                "bytes": destination.stat().st_size,
                "source_archive_matches_lock": True,
                "archive_cache_relative_path": registry_name + "/" + filename,
                "output_relative_path": destination.relative_to(output).as_posix(),
                "official_archive_url": f"https://static.crates.io/crates/{record['name']}/{filename}" if crates_io else None,
                "origin": "Unmodified local Cargo cache .crate, copied without extraction or downloading.",
            })
        except OSError as exc:
            issue(record, "mpl_source_archive_missing_or_unreadable", f"{filename}: {exc}")
    result: dict[str, Any] = {
        "schema_version": 1,
        "scope": "Exact cached source archives for Cargo packages declaring MPL and observed in the supplied completed build-input directory.",
        "packages": sources,
        "download_performed": False,
    }
    if sources:
        manifest_path = output / "licenses" / "third-party-sources" / "manifest.json"
        manifest_bytes = (json.dumps(result, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
        atomic_write(manifest_path, manifest_bytes)
        result["manifest_relative_path"] = manifest_path.relative_to(output).as_posix()
        result["manifest_sha256"] = digest(manifest_bytes)
    return result


def collect(project: Path, cargo_home: Path, output: Path, overrides: Path | None = None, build_inputs: Path | None = None) -> dict[str, Any]:
    project, cargo_home, output = project.resolve(), cargo_home.resolve(), output.resolve()
    if overrides is not None and not overrides.is_file():
        raise ValueError(f"Explicit license override index does not exist: {overrides}")
    # Writing under dependency trees could mutate the very sources being audited.
    for protected in (project / "node_modules", cargo_home / "registry"):
        if within(output, protected):
            raise ValueError(f"Output must not be inside dependency sources: {protected}")
    if (output / "licenses").is_symlink():
        raise ValueError("Output licenses directory must not be a symlink.")
    output.mkdir(parents=True, exist_ok=True)
    cargo = collect_cargo(project, cargo_home, output)
    npm = collect_npm(project, output)
    records = cargo["packages"] + npm["packages"]
    override_result = apply_overrides(records, overrides or (project / "scripts" / "license-overrides" / "index.json"), output)
    build_evidence, observed_sources = inspect_build_inputs(build_inputs)
    for item in cargo["packages"]:
        item["observed_in_build_inputs"] = item["name"] + "-" + item["version"] in observed_sources if build_evidence["available"] else None
    third_party_sources = collect_mpl_sources(cargo["packages"], cargo_home, output)
    missing = [
        {"ecosystem": item["ecosystem"], "id": item["id"], "name": item["name"], "version": item["version"], "observed_in_build_inputs": item.get("observed_in_build_inputs"), "issues": item["issues"]}
        for item in records if item["issues"]
    ]
    for dependency in npm["unresolved_dependencies"]:
        if not dependency["optional"]:
            missing.append({"ecosystem": "npm", "name": dependency["name"], "issues": [{"code": "unresolved_required_dependency", "detail": dependency}]})
    license_files = {file["output_relative_path"] for item in records for file in item["license_files"]}
    expected_files = license_files | {item["output_relative_path"] for item in third_party_sources["packages"]}
    if "manifest_relative_path" in third_party_sources:
        expected_files.add(third_party_sources["manifest_relative_path"])
    observed_missing = [item for item in missing if item.get("observed_in_build_inputs") or item["ecosystem"] == "npm"]
    stale_files = sorted(
        path.relative_to(output).as_posix()
        for path in (output / "licenses").rglob("*")
        if path.is_file() and path.relative_to(output).as_posix() not in expected_files
    )
    result = {
        "schema_version": 1,
        "generated_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "offline": True,
        "cargo_was_invoked": False,
        "license_inference_performed": False,
        "notes": [
            "License metadata and existing texts are collected as found; this is not a legal-compliance determination.",
            "No references, OpenSteamTool baseline, paid Toolbox files, Steam installation files, or account data are scanned or copied.",
            "Cargo includes a lockfile superset. Missing uninstalled target-only crates do not by themselves identify missing licenses in the actual Windows executable.",
            "Existing output files are not deleted. stale_files are outside this run's manifest and should not be treated as current collected dependencies.",
        ],
        "cargo": cargo,
        "npm": npm,
        "offline_overrides": override_result,
        "build_input_evidence": build_evidence,
        "third_party_sources": third_party_sources,
        "missing": missing,
        "missing_observed_build_or_npm": observed_missing if build_evidence["available"] else None,
        "stale_files": stale_files,
        "summary": {
            "cargo_packages": len(cargo["packages"]),
            "cargo_workspace_or_path_skipped": len(cargo["skipped_packages"]),
            "npm_packages": len(npm["packages"]),
            "license_files": len(license_files),
            "third_party_source_archives": len(third_party_sources["packages"]),
            "packages_with_issues": len(missing),
            "stale_files": len(stale_files),
            "official_upstream_override_packages": override_result["applied_packages"],
            "observed_build_or_npm_packages_with_issues": len(observed_missing) if build_evidence["available"] else None,
        },
    }
    atomic_write(output / "dependency-licenses.json", (json.dumps(result, ensure_ascii=False, indent=2) + "\n").encode("utf-8"))
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="Output directory; writes licenses/ and dependency-licenses.json without deleting existing files.")
    parser.add_argument("--project-root", type=Path, default=Path(__file__).resolve().parents[1], help="Directory containing Cargo.lock, package-lock.json and node_modules (default: this project).")
    parser.add_argument("--cargo-home", type=Path, default=Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))), help="Local Cargo home (default: CARGO_HOME or ~/.cargo).")
    parser.add_argument("--strict", action="store_true", help="Exit 2 when missing/invalid items or stale output files are reported; the manifest is still written.")
    parser.add_argument("--overrides", type=Path, help="Optional local exact-version upstream license index (default: project scripts/license-overrides/index.json when present). No networking is performed.")
    parser.add_argument("--build-inputs", type=Path, help="Completed target/release directory whose rustc .d files tag observed build inputs. Copies cached MPL source archives; exits 2 for issues in observed build/npm scope. Not an exact link-time graph.")
    args = parser.parse_args(argv)
    try:
        result = collect(args.project_root, args.cargo_home, args.output, args.overrides, args.build_inputs)
    except (OSError, ValueError, RuntimeError) as exc:
        print(f"collect_licenses: {exc}", file=sys.stderr)
        return 1
    print(json.dumps({"manifest": str(args.output.resolve() / "dependency-licenses.json"), **result["summary"]}, ensure_ascii=False))
    observed_issues = result["summary"]["observed_build_or_npm_packages_with_issues"]
    if args.build_inputs and (not result["build_input_evidence"]["available"] or observed_issues):
        print("collect_licenses: observed build/npm license or MPL-source issues remain, or no build evidence was found; see manifest.", file=sys.stderr)
        return 2
    if args.strict and (result["missing"] or result["stale_files"]):
        return 2
    if result["missing"]:
        scope = "outside observed build/npm scope" if args.build_inputs else "in the lockfile superset (no build evidence supplied)"
        print(f"collect_licenses: WARNING: {len(result['missing'])} package(s) with unresolved items {scope}; details retained in manifest.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
