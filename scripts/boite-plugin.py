#!/usr/bin/env python3
"""Writes boite-plugin.json for a release: Boite's plugin manifest.

    python3 scripts/boite-plugin.py <version> <dir-with-executables> [--output <file>]

The directory holds the raw executables a release attaches
(douane-windows-x64.exe, douane-macos-arm64 and so on). Fields other than the
version and artifacts come from the existing boite-plugin.json when there is
one, otherwise from the defaults below. The result is checked against the
rules Boite applies (docs/plugins.md in mtsu-dev/boite) before it is written:
an unknown field or a bad value stops the script with the field it names.
"""

import argparse
import hashlib
import json
import os
import re
import sys
from urllib.parse import urlsplit

REPOSITORY = "https://github.com/mtsu-dev/douane"
# Boite platform key -> release asset name.
ASSETS = {
    "win32-x64": "douane-windows-x64.exe",
    "darwin-arm64": "douane-macos-arm64",
    "darwin-x64": "douane-macos-x64",
    "linux-x64": "douane-linux-x64",
    "linux-arm64": "douane-linux-arm64",
}
DEFAULTS = {
    "schema": 1,
    "id": "douane",
    "name": "Douane",
    "description": (
        "Local gateway for AI coding subscriptions: pools Claude, Codex, Grok, "
        "Copilot and API key accounts behind one OpenAI and Anthropic compatible "
        "endpoint, with quota tracking and a dashboard."
    ),
    "homepage": REPOSITORY,
    "executable": "douane",
    "provides": {"gateway": {}},
}

FIELDS = ["schema", "id", "name", "version", "description", "homepage", "executable", "artifacts", "provides"]
PLATFORMS = ["win32-x64", "win32-arm64", "darwin-x64", "darwin-arm64", "linux-x64", "linux-arm64"]
FEATURES = ["accountPools", "desktopApp", "gateway"]
POOL_PROVIDERS = ["antigravity", "antigravity-cli", "claude", "codex", "grok", "muse", "opencode", "opencode-v2", "pi"]
ID = re.compile(r"^[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$")
VERSION = re.compile(r"^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$")
EXECUTABLE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
CONTROL = re.compile(r"[\x00-\x1f\x7f]")


def problems(manifest):
    """Every rule Boite's reader refuses, as `field must be expected` lines."""
    found = []

    def refuse(field, expected):
        found.append(f"boite-plugin.json: {field} must be {expected}")

    def text(value, field, limit):
        if not isinstance(value, str) or not value.strip() or len(value) > limit or CONTROL.search(value):
            refuse(field, f"a string of 1 to {limit} characters, no control characters")

    def https(value, field):
        parts = urlsplit(value) if isinstance(value, str) else None
        if (
            parts is None
            or len(value) > 2048
            or parts.scheme != "https"
            or not parts.hostname
            or parts.username is not None
            or parts.password is not None
        ):
            refuse(field, "an https URL with a host and no credentials")

    def empty(value, field):
        if not isinstance(value, dict) or value:
            refuse(field, "an empty object")

    if not isinstance(manifest, dict):
        return ["boite-plugin.json: (root) must be a JSON object"]
    for key in manifest:
        if key not in FIELDS:
            refuse(key, f"one of {', '.join(FIELDS)}")
    for key in FIELDS:
        if key not in manifest:
            refuse(key, "present")
    if manifest.get("schema") != 1:
        refuse("schema", "1")
    if not isinstance(manifest.get("id"), str) or not ID.match(manifest["id"]):
        refuse("id", "lowercase letters, digits and inner hyphens, 1 to 64 characters")
    text(manifest.get("name"), "name", 60)
    if not isinstance(manifest.get("version"), str) or not VERSION.match(manifest["version"]):
        refuse("version", "a version such as 1.2.0 or 1.2.0-beta.1")
    text(manifest.get("description"), "description", 300)
    https(manifest.get("homepage"), "homepage")
    if not isinstance(manifest.get("executable"), str) or not EXECUTABLE.match(manifest["executable"]):
        refuse("executable", "a file name of letters, digits, _ and -, no extension")
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, dict) or not artifacts:
        refuse("artifacts", f"an object keyed by at least one of {', '.join(PLATFORMS)}")
    else:
        for platform, artifact in artifacts.items():
            field = f"artifacts.{platform}"
            if platform not in PLATFORMS:
                refuse(field, f"one of {', '.join(PLATFORMS)}")
            elif not isinstance(artifact, dict) or set(artifact) != {"url", "sha256"}:
                refuse(field, "an object with url and sha256 only")
            else:
                https(artifact["url"], f"{field}.url")
                if not isinstance(artifact["sha256"], str) or not SHA256.match(artifact["sha256"]):
                    refuse(f"{field}.sha256", "64 lowercase hexadecimal characters")
    provides = manifest.get("provides")
    if not isinstance(provides, dict) or not provides:
        refuse("provides", f"an object naming at least one of {', '.join(FEATURES)}")
    else:
        for feature, value in provides.items():
            field = f"provides.{feature}"
            if feature not in FEATURES:
                refuse(field, f"one of {', '.join(FEATURES)}")
            elif feature == "accountPools":
                pools = value.get("providers") if isinstance(value, dict) and set(value) == {"providers"} else None
                if (
                    not isinstance(pools, list)
                    or not pools
                    or len(set(map(str, pools))) != len(pools)
                    or any(p not in POOL_PROVIDERS for p in pools)
                ):
                    refuse(f"{field}.providers", f"a non-empty array of distinct ids among {', '.join(POOL_PROVIDERS)}")
            else:
                empty(value, field)
    return found


def build(base, version, directory):
    artifacts = {}
    for platform, asset in ASSETS.items():
        path = os.path.join(directory, asset)
        if not os.path.isfile(path):
            continue
        with open(path, "rb") as file:
            digest = hashlib.sha256(file.read()).hexdigest()
        artifacts[platform] = {"url": f"{REPOSITORY}/releases/download/v{version}/{asset}", "sha256": digest}
    manifest = {key: base.get(key, DEFAULTS.get(key)) for key in FIELDS}
    manifest["version"] = version
    manifest["artifacts"] = artifacts
    return manifest


def main():
    parser = argparse.ArgumentParser(description="Write boite-plugin.json for a release.")
    parser.add_argument("version")
    parser.add_argument("directory")
    parser.add_argument("--output", default="boite-plugin.json")
    args = parser.parse_args()
    base = {}
    if os.path.isfile("boite-plugin.json"):
        with open("boite-plugin.json", encoding="utf-8") as file:
            base = json.load(file)
    manifest = build(base, args.version, args.directory)
    missing = [asset for platform, asset in ASSETS.items() if platform not in manifest["artifacts"]]
    if missing:
        print(f"missing release executables: {', '.join(missing)}", file=sys.stderr)
        return 1
    found = problems(manifest)
    if found:
        print("\n".join(found), file=sys.stderr)
        return 1
    with open(args.output, "w", encoding="utf-8") as file:
        json.dump(manifest, file, indent=2)
        file.write("\n")
    print(f"{args.output}: {args.version}, {len(manifest['artifacts'])} platforms")
    return 0


if __name__ == "__main__":
    sys.exit(main())
