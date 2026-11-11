"""Regression tests for the license gate used before publishing distributions."""

from __future__ import annotations

import importlib.util
import io
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parents[1] / "check_release_artifacts.py"
SPEC = importlib.util.spec_from_file_location("check_release_artifacts", MODULE_PATH)
assert SPEC and SPEC.loader
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


class ReleaseArtifactTests(unittest.TestCase):
    def artifact(
        self,
        package="oxyde-postgres-core",
        *,
        source=False,
        omit=None,
        version="0.1.0",
        author=True,
        windows_newlines=False,
        description="",
        requires_python=">=3.13",
    ):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / ("package.tar.gz" if source else "package.whl")
        metadata = (
            f"Name: {package}\nVersion: {version}\nRequires-Python: {requires_python}\n"
        )
        if author:
            metadata += "Author-email: Nikita Ryzhenkov <nikita.ryzhenkoff@gmail.com>\n"
        metadata += "\n" + description
        files = {
            "package/PKG-INFO"
            if source
            else "package.dist-info/METADATA": metadata.encode()
        }
        for label, contents in checker.expected_licenses(package).items():
            if label != omit:
                if windows_newlines:
                    contents = contents.replace(b"\r\n", b"\n").replace(b"\n", b"\r\n")
                files[f"package/licenses/{label}"] = contents
        if source:
            with tarfile.open(path, "w:gz") as archive:
                for name, contents in files.items():
                    member = tarfile.TarInfo(name)
                    member.size = len(contents)
                    archive.addfile(member, io.BytesIO(contents))
        else:
            with zipfile.ZipFile(path, "w") as archive:
                for name, contents in files.items():
                    archive.writestr(name, contents)
        return path

    def test_all_package_formats_preserve_licenses(self):
        for package in ("oxyde-postgres", "oxyde-postgres-core"):
            for source in (False, True):
                with self.subTest(package=package, source=source):
                    checker.validate_artifact(
                        self.artifact(package, source=source), package, "0.1.0"
                    )

    def test_missing_upstream_license_fails(self):
        for source in (False, True):
            with (
                self.subTest(source=source),
                self.assertRaisesRegex(ValueError, "Oxyde MIT"),
            ):
                checker.validate_artifact(
                    self.artifact(source=source, omit="Oxyde MIT"),
                    "oxyde-postgres-core",
                    "0.1.0",
                )

    def test_relative_description_links_fail(self):
        for source in (False, True):
            for target in (
                "../../README.md",
                "../../docs/changelog.md",
                "../../docs/guide/connections.md",
            ):
                with (
                    self.subTest(source=source, target=target),
                    self.assertRaisesRegex(ValueError, "relative description link"),
                ):
                    checker.validate_artifact(
                        self.artifact(source=source, description=f"[Guide]({target})"),
                        "oxyde-postgres-core",
                        "0.1.0",
                    )

    def test_absolute_description_links_are_allowed(self):
        checker.validate_artifact(
            self.artifact(
                description="[Oxyde for PostgreSQL](https://github.com/ebrake/oxyde-postgres)"
            ),
            "oxyde-postgres-core",
            "0.1.0",
        )

    def test_windows_line_endings_preserve_complete_license_text(self):
        for package in ("oxyde-postgres", "oxyde-postgres-core"):
            for source in (False, True):
                with self.subTest(package=package, source=source):
                    checker.validate_artifact(
                        self.artifact(package, source=source, windows_newlines=True),
                        package,
                        "0.1.0",
                    )

    def test_each_vendored_notice_is_required(self):
        for label in ("SQLx LICENSE-MIT", "SQLx LICENSE-APACHE"):
            with self.subTest(label=label), self.assertRaisesRegex(ValueError, "SQLx"):
                checker.validate_artifact(
                    self.artifact(omit=label), "oxyde-postgres-core", "0.1.0"
                )

    def test_wrong_version_fails(self):
        with self.assertRaisesRegex(ValueError, "name or version"):
            checker.validate_artifact(
                self.artifact(version="0.2.0"), "oxyde-postgres-core", "0.1.0"
            )

    def test_missing_author_fails(self):
        with self.assertRaisesRegex(ValueError, "author credit"):
            checker.validate_artifact(
                self.artifact(author=False), "oxyde-postgres-core", "0.1.0"
            )

    def test_wrong_python_requirement_fails(self):
        for package in ("oxyde-postgres", "oxyde-postgres-core"):
            for source in (False, True):
                with (
                    self.subTest(package=package, source=source),
                    self.assertRaisesRegex(ValueError, "Python requirement"),
                ):
                    checker.validate_artifact(
                        self.artifact(package, source=source, requires_python=">=3.10"),
                        package,
                        "0.1.0",
                    )


if __name__ == "__main__":
    unittest.main()
