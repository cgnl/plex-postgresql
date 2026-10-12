"""Guard build sharing without reducing native acceptance coverage."""
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[2] / '.github/workflows/docker-publish.yml'


class NativeImageMatrix(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()
        self.build = self.workflow.split('\n  candidate-image:\n')[1].split('\n  candidate:\n')[0]
        self.candidate = self.workflow.split('\n  candidate:\n')[1].split('\n  soak:\n')[0]

    def test_four_native_builds_feed_eight_postgres_test_lanes(self):
        rows = re.findall(r'- variant: (\w+)\n\s+dockerfile: [^\n]+\n\s+arch: (\w+)\n\s+runner: ([^\n]+)', self.build)
        self.assertEqual(len(rows), 4)
        self.assertEqual({(v, a) for v, a, _ in rows},
                         {(v, a) for v in ('linuxserver', 'plexinc') for a in ('amd64', 'arm64')})
        for _, arch, runner in rows:
            self.assertEqual(runner, 'ubuntu-24.04-arm' if arch == 'arm64' else 'ubuntu-24.04')
        self.assertNotIn('postgres:', self.build)
        self.assertEqual(self.candidate.count('postgres: 15'), 4)
        self.assertEqual(self.candidate.count('postgres: 18'), 4)
        self.assertIn('strategy: *native_strategy', self.workflow.split('\n  soak:\n')[1])

    def test_test_jobs_cannot_build_or_push_images(self):
        self.assertIn('packages: write', self.build)
        self.assertIn('packages: read', self.candidate)
        self.assertNotIn('packages: write', self.candidate)
        self.assertNotIn('docker/build-push-action', self.candidate)
        self.assertNotIn('docker/setup-buildx-action', self.candidate)
        self.assertIn('needs: [resolve, linux-bundle]', self.build)
        self.assertIn('needs: [resolve, media-fixtures, candidate-image]', self.candidate)
        self.assertEqual(self.build.count('docker/build-push-action'), 1)
        self.assertNotIn('docker pull', self.build)

    def test_historical_payload_skip_does_not_skip_successful_consumers(self):
        # Historical profiles reuse the latest payload, so linux-payload is
        # deliberately skipped while linux-bundle completes. Every consumer
        # must override Actions' implicit ancestor success() with always(),
        # while still requiring every direct dependency to have succeeded.
        dependencies = {
            'candidate-image': ('resolve', 'linux-bundle'),
            'candidate': ('resolve', 'media-fixtures', 'candidate-image'),
            'native-bundle-install': ('resolve', 'linux-bundle'),
        }
        for job, required in dependencies.items():
            with self.subTest(job=job):
                block = self.workflow.split('\n  ' + job + ':\n')[1].split('    steps:', 1)[0]
                condition = re.search(r'^    if: \$\{\{ (.+) \}\}$', block, re.MULTILINE).group(1)
                self.assertEqual(condition, 'always() && ' + ' && '.join(
                    "needs." + dependency + ".result == 'success'" for dependency in required))
                self.assertNotIn('linux-payload.result', condition)

    def test_existing_certificate_lane_shape_and_byte_binding_remain(self):
        self.assertIn('built-image-${{ matrix.variant }}-${{ matrix.arch }}', self.build)
        self.assertIn('built-image-${{ matrix.variant }}-${{ matrix.arch }}', self.candidate)
        self.assertIn("inputs.payload_profile_id || inputs.profile_id", self.build)
        for field in ('candidate-ref.txt', 'build-metadata.json', 'source-sha.txt'):
            self.assertIn(field, self.build)
        self.assertIn('verify_candidate_payload.py --bundle', self.candidate)
        self.assertIn('--output evidence/payload-binding.json', self.candidate)
        self.assertIn('candidate-${{ matrix.variant }}-${{ matrix.arch }}-pg${{ matrix.postgres }}', self.candidate)
        self.assertIn("inputs.mode == 'probe' && '1' || '100'", self.candidate)
        self.assertIn('21000', self.workflow)
        self.assertNotIn('steps.build.outputs', self.candidate)

    def test_consumer_rejects_tags_other_repository_and_wrong_source(self):
        block = self.candidate.split('- name: Require the shared immutable image on the resolved source\n')[1]
        script = textwrap.dedent(block.split('        run: |\n')[1].split('      - uses:')[0])
        image = 'ghcr.io/cgnl/plex-postgresql-plexinc@sha256:' + 'a' * 64
        sha = 'b' * 40
        for ref, source, ok in [(image, sha, True), (image.replace('@sha256:' + 'a' * 64, ':latest'), sha, False),
                                (image.replace('/cgnl/', '/other/'), sha, False), (image, 'c' * 40, False)]:
            with self.subTest(ref=ref, source=source), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp); (root / 'evidence').mkdir()
                (root / 'evidence/candidate-ref.txt').write_text(ref)
                (root / 'evidence/source-sha.txt').write_text(source)
                env = dict(os.environ, SOURCE_SHA=sha, REPOSITORY_OWNER='cgnl', VARIANT='plexinc', GITHUB_ENV=str(root / 'env'))
                result = subprocess.run(['bash', '-e', '-c', script], cwd=tmp, env=env, capture_output=True)
                self.assertEqual(result.returncode == 0, ok, result.stderr)
                if ok:
                    self.assertEqual((root / 'env').read_text(), 'CANDIDATE_IMAGE=' + image + '\n')


if __name__ == '__main__':
    unittest.main()
