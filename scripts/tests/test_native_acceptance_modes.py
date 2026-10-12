import os
import subprocess
import textwrap
import unittest
from pathlib import Path


class NativeAcceptanceModes(unittest.TestCase):
    def setUp(self):
        self.workflow = (Path(__file__).resolve().parents[2] /
                         '.github/workflows/docker-publish.yml').read_text()
        block = self.workflow.split('- name: Verify every native matrix lane completed\n', 1)[1]
        block = block.split('- uses:', 1)[0]
        self.script = textwrap.dedent(block.split('        run: |\n', 1)[1])

    def gate(self, mode, soak, **overrides):
        values = dict(RESOLUTION='success', RUNTIME='success', CANDIDATES='success',
                      SOAK=soak, BUNDLES='success', MEDIA='success', MODE=mode)
        values.update(overrides)
        return subprocess.run(['bash', '-e', '-c', self.script],
                              env=dict(os.environ, **values), capture_output=True).returncode

    def test_probe_requires_successful_workload_but_no_soak(self):
        self.assertEqual(self.gate('probe', 'skipped'), 0)
        self.assertNotEqual(self.gate('probe', 'failure'), 0)
        self.assertNotEqual(self.gate('probe', 'skipped', CANDIDATES='failure'), 0)
        self.assertNotEqual(self.gate('probe', 'skipped', BUNDLES='skipped'), 0)

    def test_full_certification_cannot_skip_soak(self):
        self.assertEqual(self.gate('full', 'success'), 0)
        self.assertNotEqual(self.gate('full', 'skipped'), 0)
        self.assertNotEqual(self.gate('full', 'failure'), 0)
        self.assertNotEqual(self.gate('unknown', 'success'), 0)

    def test_probe_never_schedules_the_soak_job(self):
        soak = self.workflow.split('\n  soak:\n', 1)[1].split('\n    needs:', 1)[0]
        self.assertIn("inputs.mode != 'probe'", soak)


if __name__ == '__main__':
    unittest.main()
