import hashlib
from pathlib import Path
import runpy
import shutil
import tempfile
import unittest
from unittest.mock import patch


nightly = runpy.run_path(str(Path(__file__).with_name("nightly.py")))
publish = nightly["publish"]
VERSION = "0.1.0-nightly.20261003.gaaaaaaaaaaaa"
SHA = "a" * 40


class PublishingTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        for platform in ("linux-x64", "linux-arm64", "darwin-arm64", "darwin-x64", "windows-x64"):
            archive = self.directory / f"chunk-{VERSION}-{platform}.tar.gz"
            archive.write_bytes(platform.encode())
            archive.with_name(archive.name + ".sha256").write_text(
                f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
        self.calls = []
        github = patch.dict(publish.__globals__, gh=self.gh, releases=lambda: [])
        github.start()
        self.addCleanup(github.stop)

    def gh(self, *args):
        self.calls.append(args)
        if args[:2] == ("release", "download"):
            destination = Path(args[args.index("--dir") + 1])
            for asset in self.directory.iterdir():
                shutil.copyfile(asset, destination / asset.name)
        return ""

    def test_complete_platform_set_is_verified_and_published(self):
        publish(self.directory, VERSION, SHA)
        self.assertIn(("release", "edit", f"v{VERSION}", "--draft=false", "--prerelease", "--latest=false"),
                      self.calls)

    def test_missing_archive_or_checksum_is_rejected_before_github_calls(self):
        for suffix in (".tar.gz", ".tar.gz.sha256"):
            with self.subTest(suffix=suffix):
                asset = self.directory / f"chunk-{VERSION}-linux-x64{suffix}"
                contents = asset.read_bytes()
                asset.unlink()
                with self.assertRaisesRegex(ValueError, "nightly assets differ: missing"):
                    publish(self.directory, VERSION, SHA)
                self.assertEqual(self.calls, [])
                asset.write_bytes(contents)

    def test_unexpected_archive_is_rejected_before_github_calls(self):
        (self.directory / "chunk-wrong-version-linux-x64.tar.gz").write_bytes(b"stale archive")
        with self.assertRaisesRegex(ValueError, "unexpected.*chunk-wrong-version"):
            publish(self.directory, VERSION, SHA)
        self.assertEqual(self.calls, [])


if __name__ == "__main__":
    unittest.main()
