"""Checks that the managed smoke's cleanup keeps going past individual failures. Run with `python3 -m unittest`."""
import importlib.util
from pathlib import Path
import shutil
import subprocess
import unittest

spec = importlib.util.spec_from_file_location('managed_smoke', Path(__file__).resolve().parents[1] / 'managed-smoke.py')
managed_smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(managed_smoke)


class FakeEngine(managed_smoke.Smoke):
    """An engine holding two recorded containers, a volume, a network and a tag, where `rm c1` times out and listing
    volumes and inspecting images fail."""

    def __init__(self):
        super().__init__('podman', Path('/nonexistent.sock'), 0)
        self.existing = {'container': {'c1', 'c2'}, 'volume': {'v1'}, 'network': {'n1'}}
        self.created.update(container={'c1', 'c2'}, volume={'v1'}, network={'n1'}, tag={'t1'})
        self.engine_locked = True
        self.calls = []

    def run(self, argv, log=None, timeout=1800, check=True, **kwargs):
        argv = list(argv[1:])
        self.calls.append(argv)
        if argv == ['rm', '-f', '-v', 'c1']:
            raise subprocess.TimeoutExpired(argv, timeout)
        if argv[:2] == ['volume', 'ls']:
            raise managed_smoke.Failed('volume ls exited 125')
        if argv[:2] == ['image', 'inspect']:
            return subprocess.CompletedProcess(argv, 125, '', 'Error: cannot connect to Podman')
        stdout = ''
        for kind, listing in managed_smoke.LISTINGS.items():
            if tuple(argv) == listing:
                stdout = '\n'.join(self.existing[kind])
        for kind, removal in managed_smoke.REMOVALS.items():
            if tuple(argv[:-1]) == removal:
                self.existing[kind].discard(argv[-1])
        return subprocess.CompletedProcess(argv, 0, stdout, '')


class CleanupTest(unittest.TestCase):
    def test_one_failure_does_not_skip_the_rest(self):
        engine = FakeEngine()
        self.addCleanup(shutil.rmtree, engine.work)
        engine.env_file.write_text('CHUNK_OPERATOR_TOKEN=secret\n')

        failures = engine.cleanup(passed=True)

        self.assertIn(['rm', '-f', '-v', 'c2'], engine.calls)
        self.assertIn(['volume', 'rm', '-f', 'v1'], engine.calls)
        self.assertIn(['network', 'rm', 'n1'], engine.calls)
        self.assertEqual(engine.calls[-1], ['network', 'rm', managed_smoke.ENGINE_LOCK])
        self.assertEqual(engine.existing, {'container': {'c1'}, 'volume': set(), 'network': set()})
        self.assertFalse(engine.env_file.exists())
        self.assertTrue(any('removing container c1' in failure for failure in failures))
        self.assertTrue(any('listing volumes' in failure for failure in failures))
        self.assertTrue(any("still present: {'container': ['c1']}" in failure for failure in failures))
        self.assertTrue(any('removing tag t1' in failure for failure in failures))
        self.assertTrue(any('inspecting image t1' in failure for failure in failures))


if __name__ == '__main__':
    unittest.main()
