import json
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
from unittest.mock import patch


publisher = runpy.run_path(str(Path(__file__).with_name("publish-maven.py")))


class PublishingTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.sdk = Path(self.temporary.name)
        (self.sdk / "sdk.json").write_text(json.dumps({"schema": 1, "version": "0.1.0"}))
        self.artifact = self.sdk / "sdk/maven/dev/chunkzero/runtime/0.1.0/runtime-0.1.0.jar"
        self.artifact.parent.mkdir(parents=True)
        self.artifact.write_bytes(b"published jar")

    def publish(self):
        publisher["publish"](self.sdk, "a" * 32, "chunk-maven")

    @patch("subprocess.run")
    def test_existing_version_is_immutable(self, run):
        run.return_value = subprocess.CompletedProcess([], 0, '{"Metadata":{"sha256":"different"}}', "")
        with self.assertRaisesRegex(ValueError, "Refusing to replace"):
            self.publish()
        self.assertEqual(run.call_count, 1)

    @patch("subprocess.run")
    def test_retry_skips_identical_artifacts(self, run):
        checksum = publisher["digest"](self.artifact)
        run.return_value = subprocess.CompletedProcess([], 0, json.dumps({"Metadata": {"sha256": checksum}}), "")
        self.publish()
        self.assertEqual(run.call_count, 1)

    @patch("subprocess.run")
    def test_new_artifact_uses_conditional_write(self, run):
        run.side_effect = [subprocess.CompletedProcess([], 254, "", "An error occurred (404)"),
                           subprocess.CompletedProcess([], 0)]
        self.publish()
        command = run.call_args.args[0]
        self.assertEqual(command[command.index("--if-none-match") + 1], "*")
        self.assertIn(f"sha256={publisher['digest'](self.artifact)}", command)

    @patch("subprocess.run")
    def test_auth_failure_never_uploads(self, run):
        run.return_value = subprocess.CompletedProcess([], 254, "", "An error occurred (403)")
        with self.assertRaisesRegex(RuntimeError, "403"):
            self.publish()
        self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
