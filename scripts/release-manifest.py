#!/usr/bin/env python3
"""Validate the Keiki release set and write its updater manifest."""

import hashlib
import json
from pathlib import Path
import re
import sys


def build_manifest(version: str, directory: Path) -> dict:
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-beta\.\d+)?", version):
        raise ValueError(f"invalid release version: {version}")
    suffixes = (
        "linux-x86_64.tar.gz", "linux-aarch64.tar.gz",
        "macos-arm64.dmg", "macos-arm64-app.tar.gz",
    )
    expected = {f"keiki-{version}-{suffix}" for suffix in suffixes}
    missing = expected - {path.name for path in directory.iterdir() if path.is_file()}
    if missing:
        raise ValueError(f"missing release artifacts: {', '.join(sorted(missing))}")
    files = {}
    for path in sorted(directory.iterdir()):
        if path.name == "manifest.json":
            continue
        if not path.is_file() or not path.name.startswith(f"keiki-{version}-"):
            raise ValueError(f"unexpected release artifact: {path.name}")
        if not path.stat().st_size:
            raise ValueError(f"empty release artifact: {path.name}")
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        files[path.name] = {"sha256": digest.hexdigest()}
    return {"version": version, "minimumMacOS": "13.0", "files": files}


if __name__ == "__main__":
    version, directory = sys.argv[1:]
    directory = Path(directory)
    manifest = build_manifest(version, directory)
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Keiki {version}: validated {len(manifest['files'])} release artifacts")
