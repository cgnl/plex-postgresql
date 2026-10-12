import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from load_release_trust import load


class ReleaseTrustConfigTests(unittest.TestCase):
    def setUp(self):
        self.trust = dict(enabled=True, repository='owner/repo', default_branch='main',
                          workflow_id=123, policy_hash='a' * 64)

    def test_operator_variable_and_file_fallback_have_identical_hash(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'trust.json'
            path.write_text(json.dumps(self.trust))
            variable = load(path, json.dumps(self.trust), 'owner/repo', 'main')
            self.assertEqual(variable, load(path, '', 'owner/repo', 'main'))

    def test_malformed_configured_variable_never_falls_back(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'trust.json'
            path.write_text(json.dumps(self.trust))
            with self.assertRaises(ValueError):
                load(path, '{bad', 'owner/repo', 'main')
            with self.assertRaises(ValueError):
                load(path, ' ', 'owner/repo', 'main')

    def test_identity_and_enable_checks(self):
        for key, value in [('enabled', False), ('repository', 'other/repo'),
                           ('default_branch', 'untrusted'), ('workflow_id', True)]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                load('unused', json.dumps(dict(self.trust, **{key: value})), 'owner/repo', 'main')

    def test_configuration_change_between_jobs_is_rejected(self):
        _, digest = load('unused', json.dumps(self.trust), 'owner/repo', 'main')
        with self.assertRaises(ValueError):
            load('unused', json.dumps(dict(self.trust, workflow_id=456)), 'owner/repo', 'main', digest)


if __name__ == '__main__':
    unittest.main()
