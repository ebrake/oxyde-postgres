"""Verify package identity, upstream attribution, and bundled release licenses."""

from __future__ import annotations

import argparse
import re
import tarfile
import zipfile
from email.parser import BytesParser
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def expected_licenses(package: str) -> dict[str, bytes]:
    licenses = {"Oxyde MIT": (ROOT / "LICENSE").read_bytes()}
    if b"Copyright (c) 2025 Nikita Ryzhenkov" not in licenses["Oxyde MIT"]:
        raise ValueError("Original Oxyde copyright notice is missing")
    if package == "oxyde-postgres-core":
        for name in ("LICENSE-MIT", "LICENSE-APACHE"):
            licenses[f"SQLx {name}"] = (
                ROOT / "vendor/sqlx-postgres" / name
            ).read_bytes()
    return licenses


def validate_artifact(path: Path, package: str, version: str) -> None:
    if path.suffix == ".whl":
        with zipfile.ZipFile(path) as archive:
            files = {
                name: archive.read(name)
                for name in archive.namelist()
                if "license" in name.lower() or name.endswith(".dist-info/METADATA")
            }
        metadata = [
            value
            for name, value in files.items()
            if name.endswith(".dist-info/METADATA")
        ]
    elif path.name.endswith(".tar.gz"):
        with tarfile.open(path) as archive:
            files = {}
            for member in archive.getmembers():
                if member.isfile() and (
                    "license" in member.name.lower()
                    or member.name.endswith("/PKG-INFO")
                ):
                    stream = archive.extractfile(member)
                    if stream is not None:
                        files[member.name] = stream.read()
        metadata = [
            value for name, value in files.items() if name.endswith("/PKG-INFO")
        ]
    else:
        raise ValueError(f"Unsupported release artifact: {path.name}")

    if not metadata:
        raise ValueError(f"{path.name}: package metadata is missing")
    for raw in metadata:
        headers = BytesParser().parsebytes(raw)
        if headers["Name"] != package or headers["Version"] != version:
            raise ValueError(
                f"{path.name}: package name or version does not match "
                f"{package} {version}"
            )
        if "Nikita Ryzhenkov" not in str(headers.get("Author-email", "")) + str(
            headers.get("Author", "")
        ):
            raise ValueError(f"{path.name}: original author credit is missing")
    bundled = [
        value.replace(b"\r\n", b"\n").strip()
        for name, value in files.items()
        if "license" in name.lower()
    ]
    for label, content in expected_licenses(package).items():
        if content.replace(b"\r\n", b"\n").strip() not in bundled:
            raise ValueError(
                f"{path.name}: complete {label} license/copyright notice is missing"
            )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument(
        "--package", choices=["oxyde-postgres-core", "oxyde-postgres"], required=True
    )
    args = parser.parse_args()
    config = ROOT / (
        "pyproject.toml" if args.package.endswith("-core") else "python/pyproject.toml"
    )
    match = re.search(r'^version = "([^"\n]+)"$', config.read_text(), re.MULTILINE)
    if match is None:
        raise ValueError(f"A static release version is required in {config}")
    version = match.group(1)
    prefix = args.package.replace("-", "_") + "-"
    artifacts = sorted(
        path
        for path in args.directory.iterdir()
        if path.name.startswith(prefix)
        and (path.suffix == ".whl" or path.name.endswith(".tar.gz"))
    )
    if not any(path.suffix == ".whl" for path in artifacts) or not any(
        path.name.endswith(".tar.gz") for path in artifacts
    ):
        raise ValueError("A source distribution and at least one wheel are required")
    for path in artifacts:
        validate_artifact(path, args.package, version)
        print(f"Verified identity, attribution, and licenses: {path.name}")


if __name__ == "__main__":
    main()
