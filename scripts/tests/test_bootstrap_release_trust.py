import hashlib
import io
import json
import sys
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bootstrap_release_trust import bootstrap, expected_cells, read_certificate
from release_certificate import trusted_file_paths
from plex_compatibility import make_profile
from test_plex_compatibility import snapshot


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.policy = b'{"maxHistoricalProfiles":2}'
        self.profile = make_profile(snapshot())
        self.blobs = {path: 'c' * 40 for path in trusted_file_paths}
        self.run = {'id': 12, 'workflow_id': 34, 'event': 'workflow_dispatch', 'head_branch': 'main',
                    'head_sha': 'd' * 40, 'run_attempt': 1, 'status': 'completed', 'conclusion': 'success',
                    'repository': {'full_name': 'owner/repo'}, 'run_started_at': '2026-10-11T01:00:00Z',
                    'updated_at': '2026-10-11T09:00:00Z'}
        self.workflow = {'id': 34, 'path': '.github/workflows/release-certification.yml'}
        cells = expected_cells([self.profile])
        self.certificate = {'schema_version': 1, 'mode': 'full', 'candidate_source_sha': 'e' * 40,
                            'candidate_tree_sha': 'f' * 40, 'project_version': '1.3.22',
                            'policy_hash': hashlib.sha256(self.policy).hexdigest(),
                            'profile_fingerprints': [self.profile['fingerprint']], 'expected_cells': cells, 'observed_cells': cells,
                            'authorization': {'repository': 'owner/repo', 'workflow_id': 34,
                                              'workflow_revision': 'd' * 40, 'head_sha': 'd' * 40,
                                              'event': 'workflow_dispatch', 'run_id': 12, 'run_attempt': 1,
                                              'conclusion': 'success'},
                            'tested_image_digests': ['ghcr.io/owner/image@sha256:' + 'a' * 64],
                            'artifacts': [{'artifact_id': 56, 'name': 'latest-linux-release-zip',
                                           'kind': 'linux-certified-bundle', 'sha256': '1' * 64}]}
        self.artifacts = [{'id': 56, 'name': 'latest-linux-release-zip', 'expired': False,
                           'workflow_run': {'id': 12, 'head_sha': 'd' * 40},
                           'created_at': '2026-10-11T08:00:00Z', 'digest': 'sha256:' + '1' * 64}]

    def client(self):
        output = io.BytesIO()
        with zipfile.ZipFile(output, 'w') as archive:
            archive.writestr('certificate.json', json.dumps(self.certificate))
        data = output.getvalue()
        certificate_artifact = {'id': 78, 'name': 'release-certificate', 'expired': False,
                                'workflow_run': {'id': 12, 'head_sha': 'd' * 40},
                                'created_at': '2026-10-11T08:30:00Z',
                                'digest': 'sha256:' + hashlib.sha256(data).hexdigest()}
        responses = {'': {'full_name': 'owner/repo', 'default_branch': 'main'},
                     'actions/workflows/release-certification.yml': self.workflow,
                     'actions/runs/12': self.run,
                     'actions/runs/12/artifacts?per_page=100': {'artifacts': self.artifacts + [certificate_artifact]},
                     'actions/artifacts/78/zip': data,
                     'commits/' + 'e' * 40: {'sha': 'e' * 40, 'commit': {'tree': {'sha': 'f' * 40}}}}
        responses.update({f'contents/{path}?ref={self.run["head_sha"]}': {'sha': blob}
                          for path, blob in self.blobs.items()})
        class Client:
            repository = 'owner/repo'
            def get(self, path, raw=False):
                return responses[path]
        return Client()

    def prepare(self):
        return bootstrap(self.client(), 12, self.policy, {'schema_version': 1, 'profiles': []},
                         self.profile, self.blobs)

    def test_completed_real_provenance_prepares_reviewable_configuration(self):
        trust = self.prepare()
        self.assertTrue(trust['enabled'])
        self.assertEqual(len(trust['bootstrap_expected_cells']), 20)
        self.assertTrue(trust['require_current_upstream'])
        self.assertNotIn('required_profile_fingerprints', trust)
        self.assertNotIn('expected_cells', trust)
        self.assertEqual(trust['reviewed_file_blobs'], self.blobs)

    def test_failed_pr_event_or_incomplete_certificate_cannot_enable_trust(self):
        for key, value in [('conclusion', 'failure'), ('event', 'pull_request'), ('head_branch', 'proposal')]:
            with self.subTest(key=key):
                original = self.run[key]
                self.run[key] = value
                with self.assertRaises(ValueError):
                    self.prepare()
                self.run[key] = original
        self.certificate['observed_cells'].pop()
        with self.assertRaises(ValueError):
            self.prepare()

    def test_reviewed_code_mismatch_and_missing_history_fail_closed(self):
        client = self.client()
        changed = dict(self.blobs)
        changed[trusted_file_paths[0]] = '9' * 40
        with self.assertRaises(ValueError):
            bootstrap(client, 12, self.policy, {'schema_version': 1, 'profiles': []}, self.profile, changed)
        historical = make_profile(snapshot('1.42.1.10000-abc'))
        historical['availability'] = 'validated'
        with self.assertRaises(ValueError):
            bootstrap(client, 12, self.policy, {'schema_version': 1, 'profiles': [historical]}, self.profile, self.blobs)

    def test_certificate_archive_rejects_paths_and_additional_payloads(self):
        for names in (['../certificate.json'], ['certificate.json', 'run.sh']):
            output = io.BytesIO()
            with zipfile.ZipFile(output, 'w') as archive:
                for name in names:
                    archive.writestr(name, '{}')
            with self.assertRaises(ValueError):
                read_certificate(output.getvalue())


if __name__ == '__main__':
    unittest.main()
