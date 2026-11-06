"""Publish a complete verified GitHub release without exposing a partial latest.

Authentication is read only from the workflow environment. Published assets are
immutable; reruns preserve a published release bound to the same source commit.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.parse
import urllib.request

HOSTS = {"api.github.com", "uploads.github.com", "github.com", "release-assets.githubusercontent.com",
         "objects.githubusercontent.com", "github-releases.githubusercontent.com"}


def version_tuple(tag: str) -> tuple[int, ...] | None:
    if not re.fullmatch(r"v\d+(?:\.\d+){2,3}", tag):
        return None
    return tuple(int(part) for part in tag[1:].split("."))


def should_be_latest(new_tag: str, current_tag: str | None) -> bool:
    candidate = version_tuple(new_tag)
    current = version_tuple(current_tag or "")
    if candidate is None:
        raise ValueError("Release tag must have a numeric version")
    return current is None or candidate > current


class ReleaseRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, url):
        target = urllib.parse.urlparse(url)
        if target.scheme != "https" or target.hostname not in HOSTS or target.username or target.password:
            raise ValueError("GitHub release redirected outside its trusted provider")
        result = super().redirect_request(request, response, code, message, headers, url)
        if result and urllib.parse.urlparse(request.full_url).hostname != target.hostname:
            result.remove_header("Authorization")
        return result


class GitHub:
    def __init__(self, token: str):
        if not token:
            raise ValueError("GITHUB_TOKEN or GH_TOKEN must be supplied by the workflow")
        self.token = token
        self.opener = urllib.request.build_opener(ReleaseRedirects())

    def raw(self, path: str, method="GET", data=None, content_type="application/json", limit=None) -> bytes:
        url = path if path.startswith("https://") else "https://api.github.com/" + path
        target = urllib.parse.urlparse(url)
        if target.scheme != "https" or target.hostname not in {"api.github.com", "uploads.github.com"}:
            raise ValueError("Release API target is outside GitHub")
        headers = {"Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json",
                   "User-Agent": "SteamTools-Next-release", "X-GitHub-Api-Version": "2022-11-28"}
        if data is not None:
            headers["Content-Type"] = content_type
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        with self.opener.open(request, timeout=180) as response:
            value = response.read((limit + 1) if limit else -1)
        if limit and len(value) > limit:
            raise ValueError("Release API response exceeded its metadata limit")
        return value

    def request(self, path: str, method="GET", value=None):
        data = json.dumps(value).encode("utf-8") if value is not None else None
        result = self.raw(path, method, data, limit=8 * 1024 * 1024)
        return json.loads(result) if result else None


def publish(repository: str, directory: Path, kind: str, api: GitHub) -> dict:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("GITHUB_REPOSITORY must identify one owner/repository")
    manifest_name = "kernel-release.json" if kind == "kernel" else "tool-release.json"
    manifest = json.loads((directory / manifest_name).read_text(encoding="utf-8-sig"))
    version, tag, commit = manifest["version"], manifest["releaseTag"], manifest["sourceCommit"]
    if tag != "v" + version or version_tuple(tag) is None or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Release manifest identity is invalid")
    prefix = "steamtools-next-kernel" if kind == "kernel" else "steamtools-next"
    names = {f"{prefix}-{version}-windows-x64.zip", f"{prefix}-{version}-source.zip",
             manifest_name, "release-SHA256SUMS.txt"}
    if manifest["asset"]["name"] != f"{prefix}-{version}-windows-x64.zip":
        raise ValueError("Release manifest asset name is invalid")
    files = {}
    for name in names:
        path = directory / name
        if path.is_symlink() or not path.is_file():
            raise ValueError("Release asset must be a regular file")
        data = path.read_bytes()
        files[name] = (data, hashlib.sha256(data).hexdigest())
    archive, checksum = files[manifest["asset"]["name"]]
    if manifest["asset"]["sizeBytes"] != len(archive) or manifest["asset"]["sha256"] != checksum:
        raise ValueError("Release binary ZIP no longer matches its manifest")
    # A tag points to the exact version-only commit, not the maintenance branch.
    tagged = api.request(f"repos/{repository}/commits/{urllib.parse.quote(tag, safe='')}")
    if tagged["sha"] != commit:
        raise ValueError("Remote release tag is bound to different sources")
    try:
        release = api.request(f"repos/{repository}/releases/tags/{tag}")
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        release = api.request(f"repos/{repository}/releases", "POST", {
            "tag_name": tag, "target_commitish": commit, "name": f"SteamTools Next {kind} {version}",
            "draft": True, "prerelease": False,
            "body": f"Automated Windows x64 {kind} release.\n\nSource commit: `{commit}`\n\n"
                    "The manifest binds all installation files by SHA-256. The corresponding-source ZIP "
                    "and third-party license material are included. Installation integrity and runtime "
                    "compatibility remain separate checks."
        })
    if not release["draft"]:
        if release.get("target_commitish") != commit or {item["name"] for item in release["assets"]} != names:
            raise ValueError("Existing published release differs; its assets were preserved")
        return {"tag": tag, "sourceCommit": commit, "published": True, "reused": True,
                "url": release["html_url"]}
    if release.get("target_commitish") != commit:
        raise ValueError("Draft release is bound to different sources")
    uploaded = {item["name"]: item for item in release["assets"]}
    if set(uploaded) - names:
        raise ValueError("Draft contains unexpected assets; preserved for review")
    upload_url = release["upload_url"].split("{", 1)[0]
    for name in sorted(files):
        data, checksum = files[name]
        item = uploaded.get(name)
        if item and item["state"] != "uploaded" and item["size"] == 0:
            # Only a zero-byte incomplete asset in this unpublished draft is reclaimed.
            api.request(f"repos/{repository}/releases/assets/{item['id']}", "DELETE")
            item = None
        if item is None:
            result = api.raw(upload_url + "?" + urllib.parse.urlencode({"name": name}), "POST", data,
                             "application/zip" if name.endswith(".zip") else "application/octet-stream")
            item = json.loads(result)
        if item["state"] != "uploaded" or item["size"] != len(data) or item.get("digest") != "sha256:" + checksum:
            raise ValueError("Uploaded release asset failed size/SHA-256 verification")
    release = api.request(f"repos/{repository}/releases/{release['id']}")
    if {item["name"] for item in release["assets"]} != names:
        raise ValueError("Draft release is incomplete; kept unpublished")
    current = None
    try:
        current = api.request(f"repos/{repository}/releases/latest")["tag_name"]
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
    latest = should_be_latest(tag, current)
    release = api.request(f"repos/{repository}/releases/{release['id']}", "PATCH",
                          {"draft": False, "make_latest": "true" if latest else "false"})
    return {"tag": tag, "sourceCommit": commit, "published": not release["draft"],
            "madeLatest": latest, "reused": False, "url": release["html_url"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind", choices=("kernel", "tool"), required=True)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", ""))
    args = parser.parse_args()
    result = publish(args.repository, args.directory.resolve(), args.kind,
                     GitHub(os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN", "")))
    print(json.dumps(result))


if __name__ == "__main__":
    main()
