#!/usr/bin/env python3
"""Advance all Ninox workspace package and path-dependency versions together."""
import re
import sys
import tomllib
from pathlib import Path

root = Path(__file__).resolve().parent.parent
manifests = sorted((root / "crates").glob("*/Cargo.toml"))
if not manifests:
    sys.exit("no workspace manifests found")
versions = {tomllib.loads(p.read_text())["package"]["version"] for p in manifests}
if len(versions) != 1:
    sys.exit(f"workspace package versions differ: {sorted(versions)}")
old = versions.pop()
match = re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", old)
if match is None:
    sys.exit(f"expected a stable version, got {old!r}")
new = f"{match[1]}.{match[2]}.{int(match[3]) + 1}"
for path in manifests:
    contents = path.read_text()
    contents, count = re.subn(r'(?m)^(version\s*=\s*)"' + re.escape(old) + r'"', rf'\g<1>"{new}"', contents, count=1)
    if count != 1:
        sys.exit(f"could not update package version in {path}")
    # Only Ninox path dependencies are tied to the workspace release.
    contents = re.sub(r'(?m)^(ninox-[\w-]+\s*=\s*\{[^\n]*?\bversion\s*=\s*)"' + re.escape(old) + r'"', rf'\g<1>"{new}"', contents)
    path.write_text(contents)
lock = root / "Cargo.lock"
contents = lock.read_text()
contents, count = re.subn(r'(?m)(^\[\[package\]\]\nname = "ninox-[^"]+"\nversion = ")' + re.escape(old) + r'("\n)', rf'\g<1>{new}\2', contents)
if count != len(manifests):
    sys.exit(f"expected {len(manifests)} workspace lockfile entries, found {count}")
lock.write_text(contents)
print(new)
