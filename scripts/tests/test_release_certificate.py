import copy
import io
import json
import sys
import tempfile
import unittest
import zipfile
from unittest.mock import patch
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from release_certificate import authenticate, validate_workload
from stage_compatibility_release import allocate, stage
from publish_certified_release import extract_assets, preserved_main
from verify_candidate_payload import verify
from release_certificate import trusted_file_paths
from validate_certification_source import validate_candidate
from issue_release_certificate import aggregate
from promote_certified_images import promotion_commands, promote, latest_command


class CertificateTests(unittest.TestCase):
    def setUp(self):
        sha = 'a' * 40
        self.source = {'sha': sha, 'commit': {'tree': {'sha': 'b' * 40}}}
        self.trust = {'enabled': True, 'repository': 'owner/repo', 'workflow_id': 42,
                      'workflow_path': '.github/workflows/release-certification.yml',
                      'workflow_revision': 'c' * 40, 'default_branch': 'main', 'policy_hash': 'd' * 64,
                      'required_profile_fingerprints': ['e' * 64], 'expected_cells': ['latest/full/amd64']}
        auth = {'repository': 'owner/repo', 'workflow_id': 42, 'workflow_revision': 'c' * 40,
                'head_sha': 'c' * 40, 'event': 'workflow_dispatch', 'run_id': 123, 'run_attempt': 2,
                'conclusion': 'success'}
        self.certificate = {'schema_version': 1, 'mode': 'full', 'authorization': auth,
                            'candidate_source_sha': sha, 'candidate_tree_sha': 'b' * 40,
                            'project_version': '1.3.23', 'policy_hash': 'd' * 64,
                            'profile_fingerprints': ['e' * 64], 'expected_cells': ['latest/full/amd64'],
                            'observed_cells': ['latest/full/amd64'],
                            'tested_image_digests': ['ghcr.io/owner/image@sha256:' + 'f' * 64],
                            'artifacts': [{'artifact_id': 456, 'name': 'latest-linux-release-zip',
                                           'kind': 'linux-certified-bundle', 'sha256': '1' * 64}]}
        self.run = {'repository': {'full_name': 'owner/repo'}, 'workflow_id': 42, 'id': 123,
                    'run_attempt': 2, 'head_sha': 'c' * 40, 'event': 'workflow_dispatch',
                    'head_branch': 'main', 'status': 'completed', 'conclusion': 'success'}
        self.run.update(run_started_at='2026-10-11T01:00:00Z', updated_at='2026-10-11T09:00:00Z')
        self.workflow = {'id': 42, 'path': self.trust['workflow_path']}
        self.artifacts = {'artifacts': [{'id': 456, 'name': 'latest-linux-release-zip', 'expired': False,
                                         'workflow_run': {'id': 123, 'head_sha': 'c' * 40},
                                         'created_at': '2026-10-11T08:00:00Z', 'digest': 'sha256:' + '1' * 64}]}

    def check(self):
        return authenticate(self.certificate, self.trust, self.run, self.workflow, self.artifacts, self.source)

    def test_real_candidate_and_distinct_workflow_revision(self):
        self.assertTrue(self.check())

    def test_mutations_fail_closed(self):
        for section, key, value in [('run', 'event', 'pull_request'), ('run', 'head_branch', 'untrusted'),
                                    ('run', 'run_attempt', 1), ('run', 'conclusion', 'failure'),
                                    ('run', 'head_sha', '9' * 40), ('certificate', 'mode', 'probe'),
                                    ('certificate', 'candidate_source_sha', '9' * 40),
                                    ('certificate', 'observed_cells', []), ('trust', 'enabled', False)]:
            with self.subTest(section=section, key=key):
                saved = copy.deepcopy(getattr(self, section))
                getattr(self, section)[key] = value
                with self.assertRaises(ValueError):
                    self.check()
                setattr(self, section, saved)

    def test_artifact_substitution_and_expiry(self):
        self.artifacts['artifacts'][0]['expired'] = True
        with self.assertRaises(ValueError):
            self.check()
        self.artifacts['artifacts'][0]['expired'] = False
        self.artifacts['artifacts'][0]['workflow_run']['id'] = 999
        with self.assertRaises(ValueError):
            self.check()

    def test_previous_attempt_artifact_rejected(self):
        self.artifacts['artifacts'][0]['created_at'] = '2026-10-10T08:00:00Z'
        with self.assertRaises(ValueError):
            self.check()

    def test_workflow_revision_rotates_only_when_reviewed_code_is_unchanged(self):
        self.trust['reviewed_file_blobs'] = {path: '5' * 40 for path in trusted_file_paths}
        self.run['head_sha'] = '6' * 40
        self.certificate['authorization'].update(head_sha='6' * 40, workflow_revision='6' * 40)
        self.artifacts['artifacts'][0]['workflow_run']['head_sha'] = '6' * 40
        self.assertTrue(authenticate(self.certificate, self.trust, self.run, self.workflow,
                                     self.artifacts, self.source, dict(self.trust['reviewed_file_blobs'])))
        changed = dict(self.trust['reviewed_file_blobs'])
        changed['scripts/publish_certified_release.py'] = '7' * 40
        with self.assertRaises(ValueError):
            authenticate(self.certificate, self.trust, self.run, self.workflow, self.artifacts, self.source, changed)

    def test_new_latest_requires_independently_resolved_coverage(self):
        self.trust['require_current_upstream'] = True
        self.certificate['profile_fingerprints'] = ['8' * 64]
        independent = {'policy_hash': self.trust['policy_hash'], 'profile_fingerprints': ['8' * 64],
                       'expected_cells': self.trust['expected_cells']}
        with self.assertRaises(ValueError):
            self.check()
        self.assertTrue(authenticate(self.certificate, self.trust, self.run, self.workflow,
                                     self.artifacts, self.source, expected_coverage=independent))
        independent['profile_fingerprints'] = ['9' * 64]
        with self.assertRaises(ValueError):
            authenticate(self.certificate, self.trust, self.run, self.workflow,
                         self.artifacts, self.source, expected_coverage=independent)

    def test_unreviewed_candidate_cannot_reach_privileged_jobs(self):
        self.assertTrue(validate_candidate('a' * 40, 'a' * 40, [], [], 'probe'))
        self.assertTrue(validate_candidate('b' * 40, 'a' * 40, ['a' * 40], ['VERSION', 'CHANGELOG.md'], 'full'))
        for mode, parents, files in [('probe', ['a' * 40], ['VERSION']),
                                     ('full', ['c' * 40], ['VERSION']),
                                     ('full', ['a' * 40], ['scripts/publish_certified_release.py'])]:
            with self.assertRaises(ValueError):
                validate_candidate('b' * 40, 'a' * 40, parents, files, mode)

    def test_publication_can_fastforward_only_the_exact_staged_leaf(self):
        source = dict(self.source, parents=[{'sha': '2' * 40}])
        base = {'sha': '2' * 40}
        self.assertTrue(preserved_main(source, base, '2' * 40, True))
        with self.assertRaises(ValueError):
            preserved_main(source, base, '2' * 40, False)
        self.assertFalse(preserved_main(source, source, '2' * 40, True))
        merged = {'sha': '3' * 40, 'parents': [{'sha': source['sha']}, {'sha': '2' * 40}],
                  'commit': source['commit']}
        self.assertFalse(preserved_main(source, merged, '2' * 40, True))
        merged['parents'][1]['sha'] = '4' * 40
        with self.assertRaises(ValueError):
            preserved_main(source, merged, '2' * 40, True)

    def test_history_requires_current_leaf_and_identical_release_bytes(self):
        latest = dict(self.certificate, staged_base_sha='2' * 40)
        latest['profiles'] = [{'fingerprint': latest['profile_fingerprints'][0]}]
        latest['artifacts'] = [dict(latest['artifacts'][0], files={'bundle.zip': '3' * 64})]
        historical = copy.deepcopy(latest)
        historical['profile_fingerprints'] = ['4' * 64]
        historical['profiles'] = [{'fingerprint': '4' * 64}]
        historical['expected_cells'] = historical['observed_cells'] = ['old/full/amd64']
        combined = aggregate([latest, historical])
        self.assertEqual(len(combined['profile_fingerprints']), 2)
        historical['candidate_source_sha'] = '5' * 40
        with self.assertRaises(ValueError):
            aggregate([latest, historical])
        historical['candidate_source_sha'] = latest['candidate_source_sha']
        historical['artifacts'][0]['files']['bundle.zip'] = '6' * 64
        with self.assertRaises(ValueError):
            aggregate([latest, historical])

    def test_immutable_image_promotion_accepts_only_tested_digests(self):
        groups = {variant: {arch: f'ghcr.io/owner/plex-postgresql-{variant}@sha256:' + value * 64
                            for arch, value in [('amd64', '7'), ('arm64', '8')]}
                  for variant in ('linuxserver', 'plexinc')}
        certificate = dict(self.certificate, promoted_image_groups=groups,
                           tested_image_digests=[image for arches in groups.values() for image in arches.values()])
        commands = promotion_commands(certificate, 'owner')
        self.assertEqual(len(commands), 2)
        latest = latest_command(commands[0][5], 'sha256:' + '6' * 64)
        self.assertTrue(latest[5].endswith(':latest'))
        self.assertTrue(latest[6].endswith('@sha256:' + '6' * 64))
        with self.assertRaises(ValueError):
            latest_command(commands[0][5], 'mutable-latest')
        existing = {commands[0][5]: {'manifests': [{'digest': 'sha256:' + '9' * 64}]}}
        with self.assertRaises(ValueError):
            promote(certificate, 'owner', existing)
        groups['plexinc']['amd64'] = 'ghcr.io/attacker/image@sha256:' + '7' * 64
        with self.assertRaises(ValueError):
            promotion_commands(certificate, 'owner')

    def test_all_immutable_indexes_are_validated_before_any_latest_update(self):
        groups = {variant: {arch: f'ghcr.io/owner/plex-postgresql-{variant}@sha256:' + value * 64
                            for arch, value in [('amd64', '7'), ('arm64', '8')]}
                  for variant in ('linuxserver', 'plexinc')}
        certificate = dict(self.certificate, promoted_image_groups=groups,
                           tested_image_digests=[image for arches in groups.values() for image in arches.values()])
        valid = {'manifests': [{'digest': 'sha256:' + value * 64} for value in ('7', '8')]}
        existing = {command[5]: valid for command in promotion_commands(certificate, 'owner')}
        invalid = {'manifests': [{'digest': 'sha256:' + '9' * 64}]}
        with patch('promote_certified_images.subprocess.check_output',
                   side_effect=[json.dumps(valid), 'Digest: sha256:' + 'a' * 64 + '\n', json.dumps(invalid)]), \
             patch('promote_certified_images.subprocess.run') as execute:
            with self.assertRaisesRegex(ValueError, 'version index differs'):
                promote(certificate, 'owner', existing, execute=True)
            execute.assert_not_called()

    def test_unsafe_archive_rejected(self):
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, 'w') as stream:
            stream.writestr('../outside', b'hostile')
        archive.seek(0)
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                extract_assets(archive, Path(directory), {'../outside': 'bad'})

    def test_version_allocation_and_breaking_policy(self):
        self.assertEqual(allocate('1.3.22', {'v1.3.22', 'v1.3.23'}), '1.3.24')
        with self.assertRaises(ValueError):
            allocate('1.3.22', set(), True)

    def test_staging_is_idempotent_and_precedes_certification(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'VERSION').write_text('1.3.22\n')
            (root / 'CHANGELOG.md').write_text('# Changelog\n\n## [1.3.21]\n\nOld\n')
            for _ in range(2):
                stage(root, {'v1.3.22'}, '1.43.4.10903-e5521bd8c')
            self.assertEqual((root / 'CHANGELOG.md').read_text().count('## [1.3.23]'), 1)
            self.assertIn('Certification is pending', (root / 'CHANGELOG.md').read_text())

    def test_actual_container_payload_substitution_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = root / 'payload'
            payload.mkdir()
            archive = root / 'bundle.zip'
            with zipfile.ZipFile(archive, 'w') as stream:
                for name in ('libpq.so.5', 'libgcc_s.so.1'):
                    stream.writestr('libs/x86_64/' + name, b'certified-library')
                    (payload / name).write_bytes(b'certified-library')
                stream.writestr('db_interpose_pg-linux-x86_64.so', b'certified-shim')
            (payload / 'db_interpose_pg.so').write_bytes(b'certified-shim')
            self.assertIn('bundle_sha256', verify(archive, payload, 'amd64'))
            (payload / 'libpq.so.5').write_bytes(b'substituted')
            with self.assertRaises(ValueError):
                verify(archive, payload, 'amd64')

    def test_smoke_and_soak_durations_cannot_be_lowered(self):
        result = {'plex_version': '1.43.4.10903-e5521bd8c', 'variant': 'plexinc', 'arch': 'amd64',
                  'candidate': 'ghcr.io/owner/image@sha256:' + 'f' * 64,
                  'phase': 'native-workload-complete', 'exit_code': 0, 'workload_passed': True,
                  'promotion_allowed': False, 'restart_cycles_requested': 100, 'restart_cycles_completed': 100}
        for key in ('live_postgres_recovery_verified', 'decoded_fixture_verified',
                    'real_movie_and_tv_playback_verified', 'real_media_full_decode_verified',
                    'native_import_and_rollback_verified', 'four_concurrent_real_media_clients_verified'):
            result[key] = True
        inputs = {'source': 'a' * 40, 'version': result['plex_version'], 'variant': 'plexinc',
                  'arch': 'amd64', 'image': result['candidate']}
        validate_workload(result, **inputs)
        result['restart_cycles_completed'] = 99
        with self.assertRaises(ValueError):
            validate_workload(result, **inputs)
        result.update(restart_cycles_requested=1, restart_cycles_completed=1,
                      soak_seconds_requested=21000, soak_seconds_completed=20999)
        with self.assertRaises(ValueError):
            validate_workload(result, soak=True, **inputs)
        result['soak_seconds_completed'] = 21000
        validate_workload(result, soak=True, **inputs)


if __name__ == '__main__':
    unittest.main()
