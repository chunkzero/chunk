import base64
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import runpy
import tempfile
from threading import Thread
import unittest


publisher = runpy.run_path(str(Path(__file__).with_name("publish-maven.py")))


class PublishingTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repository = Path(self.temporary.name)
        self.artifact = self.repository / "dev/chunkzero/runtime/0.1.0/runtime-0.1.0.jar"
        self.artifact.parent.mkdir(parents=True)
        self.artifact.write_bytes(b"verified jar")
        self.checksum = self.artifact.with_suffix(".jar.sha256")
        self.checksum.write_text(hashlib.sha256(self.artifact.read_bytes()).hexdigest())
        self.requests = []
        self.status = 201
        fixture = self

        class Proxy(BaseHTTPRequestHandler):
            def do_PUT(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                fixture.requests.append((self.path, self.headers, body))
                self.send_response(fixture.status)
                self.end_headers()

            def log_message(self, *args):
                pass

        self.server = HTTPServer(("127.0.0.1", 0), Proxy)
        self.addCleanup(self.server.server_close)
        self.thread = Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.01})
        self.thread.start()
        self.addCleanup(self.stop_proxy)
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def stop_proxy(self):
        self.server.shutdown()
        self.thread.join()

    def publish(self, url=None):
        publisher["publish"](self.repository, "0.1.0", url or self.url, "maven-r2", "temporary-password")

    def test_stages_exact_verified_bytes_and_sidecars_with_proxy_credentials(self):
        self.publish()
        self.assertEqual(len(self.requests), 2)
        authorization = base64.b64encode(b"maven-r2:temporary-password").decode()
        for (key, headers, body), artifact in zip(self.requests, (self.artifact, self.checksum)):
            self.assertEqual(key, "/" + artifact.relative_to(self.repository).as_posix())
            self.assertEqual(body, artifact.read_bytes())
            self.assertEqual(headers["Authorization"], f"Basic {authorization}")

    def test_proxy_failure_stops_uploading_and_fails_the_publication_command(self):
        for status in (302, 401, 409, 503):
            with self.subTest(status=status):
                self.requests.clear()
                self.status = status
                with self.assertRaisesRegex(RuntimeError, f"HTTP {status}"):
                    self.publish()
                self.assertEqual(len(self.requests), 1)

    def test_invalid_artifacts_are_rejected_before_any_upload(self):
        for key in ("other/group/0.1.0/a.jar", "dev/chunkzero/runtime/0.2.0/a.jar",
                    "dev/chunkzero/runtime/0.1.0/invalid name.jar"):
            with self.subTest(key=key):
                invalid = self.repository / key
                invalid.parent.mkdir(parents=True, exist_ok=True)
                invalid.write_bytes(b"invalid")
                with self.assertRaisesRegex(ValueError, "immutable versioned Chunk artifact"):
                    self.publish()
                invalid.unlink()
                self.assertEqual(self.requests, [])

    def test_symlinked_files_and_directories_are_rejected(self):
        link = self.repository / "link"
        for target in (self.artifact, self.artifact.parent):
            with self.subTest(target=target):
                link.symlink_to(target)
                with self.assertRaisesRegex(ValueError, "Symlink"):
                    self.publish()
                link.unlink()
                self.assertEqual(self.requests, [])

    def test_credentials_are_only_sent_to_the_local_proxy(self):
        for url in ("https://maven.chunkzero.com", "http://example.com:80", self.url + "/remote"):
            with self.subTest(url=url):
                with self.assertRaisesRegex(ValueError, "local Maven R2 publication proxy"):
                    self.publish(url)
                self.assertEqual(self.requests, [])


if __name__ == "__main__":
    unittest.main()
