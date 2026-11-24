#!/usr/bin/env python3
"""Consistency checks for packaging/, run by CI from the repository root.

- The .deb and .rpm metadata in packaging/ matches what the server crate's manifest carries
  (cargo-deb and cargo-generate-rpm only read the crate manifest).
- The Rust version the Dockerfile installs matches rust-toolchain.toml.
- Every image reference in the Dockerfile and compose files is pinned by digest (except
  Trackside's own, and the local name compose.build.yaml builds).

Exit status 1 lists every problem. With --allow-missing-metadata, a manifest without the
package tables is a warning instead (until the server crate adopts them).
"""

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PACKAGING = ROOT / "packaging"
CRATE_MANIFEST = ROOT / "crates" / "trackside-server" / "Cargo.toml"


def load(path):
    with path.open("rb") as f:
        return tomllib.load(f)


def check_metadata(problems, warnings, allow_missing):
    manifest = load(CRATE_MANIFEST).get("package", {}).get("metadata", {})
    for tool, source in (("deb", "deb/metadata.toml"), ("generate-rpm", "rpm/metadata.toml")):
        expected = load(PACKAGING / source)["package"]["metadata"][tool]
        actual = manifest.get(tool)
        if actual is None:
            message = f"{CRATE_MANIFEST.relative_to(ROOT)} has no [package.metadata.{tool}]; copy it from packaging/{source}"
            (warnings if allow_missing else problems).append(message)
        elif actual != expected:
            problems.append(f"[package.metadata.{tool}] in the crate manifest differs from packaging/{source}")


def check_rust_version(problems):
    channel = load(ROOT / "rust-toolchain.toml")["toolchain"]["channel"]
    dockerfile = (PACKAGING / "docker" / "Dockerfile").read_text()
    match = re.search(r"^ARG RUST_VERSION=(\S+)$", dockerfile, re.MULTILINE)
    if not match:
        problems.append("Dockerfile has no ARG RUST_VERSION")
    elif match.group(1) != channel:
        problems.append(f"Dockerfile RUST_VERSION {match.group(1)} differs from rust-toolchain.toml {channel}")


IMAGE_LINE = re.compile(r"^\s*(?:ARG \w+_(?:IMAGE|BASE)=|image:\s*)(\S+)", re.MULTILINE)


def check_pins(problems):
    files = [PACKAGING / "docker" / "Dockerfile", *sorted(PACKAGING.glob("compose*.yaml"))]
    for path in files:
        for ref in IMAGE_LINE.findall(path.read_text()):
            # Our own images, and the name compose.build.yaml gives a local build.
            if ref.startswith(("ghcr.io/slukedev/trackside:", "trackside:")):
                continue
            if not re.search(r"@sha256:[0-9a-f]{64}$", ref):
                problems.append(f"{path.relative_to(ROOT)}: {ref} is not pinned by digest")


def main():
    allow_missing = "--allow-missing-metadata" in sys.argv[1:]
    problems, warnings = [], []
    check_metadata(problems, warnings, allow_missing)
    check_rust_version(problems)
    check_pins(problems)
    for w in warnings:
        print(f"::warning::{w}")
    for p in problems:
        print(f"::error::{p}")
    if problems:
        return 1
    print("packaging checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
