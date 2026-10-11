import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from publish_v1_3_22_images import SOURCE, release_inputs
from promote_certified_images import promotion_commands


class ManualReleaseTests(unittest.TestCase):
    def fixture(self, root):
        for variant in ('linuxserver', 'plexinc'):
            for arch in ('amd64', 'arm64'):
                for pg in (15, 18):
                    lane = root / f'candidate-{variant}-{arch}-pg{pg}'
                    (lane / 'container').mkdir(parents=True)
                    image = f'ghcr.io/cgnl/plex-postgresql-{variant}@sha256:' + 'a' * 64
                    (lane / 'source-sha.txt').write_text(SOURCE)
                    (lane / 'candidate-ref.txt').write_text(image)
                    result = dict(plex_version='1.43.4.10903-e5521bd8c', variant=variant,
                                  arch=arch, candidate=image, phase='native-workload-complete',
                                  exit_code=0, workload_passed=True, promotion_allowed=False,
                                  restart_cycles_requested=100, restart_cycles_completed=100)
                    for key in ('live_postgres_recovery_verified', 'decoded_fixture_verified',
                                'real_movie_and_tv_playback_verified', 'real_media_full_decode_verified',
                                'native_import_and_rollback_verified', 'four_concurrent_real_media_clients_verified'):
                        result[key] = True
                    (lane / 'container/result.json').write_text(json.dumps(result))

    def test_all_native_cells_required(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            inputs = release_inputs(root)
            self.assertEqual(len(inputs['tested_image_digests']), 8)
            self.assertEqual(len(promotion_commands(inputs, 'cgnl')), 2)
            (root / 'candidate-plexinc-arm64-pg18/source-sha.txt').write_text('b' * 40)
            with self.assertRaises(ValueError):
                release_inputs(root)

    def test_incomplete_restart_record_cannot_publish(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / 'candidate-linuxserver-amd64-pg15/container/result.json'
            result = json.loads(path.read_text())
            result['restart_cycles_completed'] = 99
            path.write_text(json.dumps(result))
            with self.assertRaises(ValueError):
                release_inputs(root)


if __name__ == '__main__':
    unittest.main()
