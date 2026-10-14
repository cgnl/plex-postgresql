import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import plex_compatibility as compatibility


def tree(entries):
    return {"truncated": False, "tree": [{"path": path, "mode": "100644", "type": "blob", "sha": sha}
                                         for path, sha in entries.items()]}


class ReleaseDedupeTests(unittest.TestCase):
    def test_release_prose_does_not_change_runtime(self):
        old = tree({"VERSION": "a", "README.md": "a", "CHANGELOG.md": "a", "docs/status.md": "a", "rust/lib.rs": "a"})
        new = tree({"VERSION": "b", "README.md": "b", "CHANGELOG.md": "b", "docs/status.md": "b", "rust/lib.rs": "a"})
        self.assertEqual(compatibility.release_tree(old), compatibility.release_tree(new))

    def test_executable_fixture_and_workflow_changes_are_not_ignored(self):
        for path in ("rust/lib.rs", "scripts/plex_compatibility.py", ".github/workflows/check-updates.yml", "scripts/media-fixtures.json", "docs/fixture.json"):
            with self.subTest(path=path):
                self.assertNotEqual(compatibility.release_tree(tree({path: "a"})),
                                    compatibility.release_tree(tree({path: "b"})))
        with self.assertRaises(ValueError):
            compatibility.release_tree({"truncated": True, "tree": []})

    def test_unconfigured_does_not_claim_published(self):
        with patch.dict(os.environ, {"RELEASE_TRUST_JSON": ""}):
            self.assertFalse(compatibility.already_published("owner/repo", {}))

    def test_published_probe_creates_neither_report_nor_dispatch(self):
        plan = {"source_sha": "a" * 40}
        with patch.object(compatibility, "already_published", return_value=True), patch.object(
                compatibility, "github_api", side_effect=[{"default_branch": "main"}, {"object": {"sha": "a" * 40}}]) as api:
            self.assertEqual(compatibility.report_pr("owner/repo", plan, "probe-passed", "url"), "already-published")
            self.assertEqual(compatibility.dispatch_followup("owner/repo", plan, "probe-passed", "url"), "already-published")
            self.assertEqual(api.call_count, 2)

    def test_unverifiable_proof_stops_instead_of_staging(self):
        with patch.object(compatibility, "already_published", side_effect=ValueError("bad proof")):
            with self.assertRaises(ValueError):
                compatibility.report_pr("owner/repo", {}, "probe-passed", "url")


if __name__ == "__main__":
    unittest.main()

class PublishedProofTests(unittest.TestCase):
    def setUp(self):
        import base64
        import hashlib
        import io
        import json
        import zipfile
        import test_release_certificate as fixtures
        from release_certificate import trusted_file_paths
        from publish_certified_release import expected_coverage
        fixture = fixtures.CertificateTests()
        fixture.setUp()
        self.run, self.certificate, self.trust = fixture.run, fixture.certificate, fixture.trust
        self.trust['bootstrap_run_id'] = 1
        self.trust['reviewed_file_blobs'] = {path: '9' * 40 for path in trusted_file_paths}
        self.trust['require_current_upstream'] = True
        policy = b'{"schema_version":1}'
        self.trust['policy_hash'] = self.certificate['policy_hash'] = hashlib.sha256(policy).hexdigest()
        profiles = [{'id': 'latest', 'fingerprint': 'e' * 64}]
        cells = expected_coverage(profiles, self.trust['policy_hash'])['expected_cells']
        self.certificate['expected_cells'] = self.certificate['observed_cells'] = cells
        self.certificate['artifacts'][0]['files'] = {'bundle.zip': '2' * 64}
        self.plan = {'source_sha': '8' * 40, 'profiles': profiles}
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, 'w') as output:
            output.writestr('certificate.json', json.dumps(self.certificate))
        data = archive.getvalue()
        proof = {'id': 789, 'name': 'release-certificate', 'expired': False,
                 'size_in_bytes': len(data), 'workflow_run': {'id': 123, 'head_sha': 'c' * 40},
                 'created_at': '2026-10-11T08:01:00Z', 'digest': 'sha256:' + hashlib.sha256(data).hexdigest()}
        self.responses = {
            'releases/latest': {'tag_name': 'v1.3.23', 'draft': False, 'prerelease': False,
                                'assets': [{'name': 'bundle.zip', 'digest': 'sha256:' + '2' * 64}]},
            'git/ref/tags/v1.3.23': {'object': {'type': 'commit', 'sha': 'a' * 40}},
            'actions/workflows/42/runs?status=success&event=workflow_dispatch&per_page=10': {'workflow_runs': [self.run]},
            'actions/runs/123/artifacts?per_page=100': {'artifacts': fixture.artifacts['artifacts'] + [proof]},
            'actions/artifacts/789/zip': data,
            'commits/' + 'a' * 40: fixture.source,
            'commits/' + '8' * 40: {'commit': {'tree': {'sha': '7' * 40}}},
            'git/trees/' + 'b' * 40 + '?recursive=1': tree({'rust/lib.rs': 'a', 'VERSION': 'a'}),
            'git/trees/' + '7' * 40 + '?recursive=1': tree({'rust/lib.rs': 'a', 'VERSION': 'b'}),
            'contents/.github/plex-compatibility-policy.json?ref=' + '8' * 40: {'content': base64.b64encode(policy).decode()},
            'actions/workflows/42': fixture.workflow,
        }
        self.calls = []

    def check(self):
        import json
        def get(path, raw=False):
            self.calls.append(path)
            if path.startswith('contents/') and '?ref=' + 'c' * 40 in path:
                return {'sha': '9' * 40}
            return self.responses[path]
        client = type('Client', (), {'get': staticmethod(get)})()
        with patch.dict(os.environ, {'RELEASE_TRUST_JSON': json.dumps(self.trust), 'GH_TOKEN': 'test'}), patch(
                'publish_certified_release.GitHub', return_value=client), patch.object(
                compatibility, 'github_api', return_value={'default_branch': 'main'}):
            return compatibility.already_published('owner/repo', self.plan)

    def test_latest_published_certificate_differs_from_bootstrap(self):
        self.assertTrue(self.check())
        self.assertNotIn('actions/runs/1', self.calls)

    def test_runtime_or_profile_change_requires_certification(self):
        self.responses['git/trees/' + '7' * 40 + '?recursive=1'] = tree({'rust/lib.rs': 'b'})
        self.assertFalse(self.check())
        self.setUp()
        self.plan['profiles'][0]['fingerprint'] = 'f' * 64
        self.assertFalse(self.check())

    def test_invalid_archive_attempt_or_asset_is_visible(self):
        for mutation in ('archive', 'attempt', 'asset'):
            with self.subTest(mutation=mutation):
                self.setUp()
                if mutation == 'archive':
                    self.responses['actions/artifacts/789/zip'] = b'changed'
                elif mutation == 'attempt':
                    self.responses['actions/runs/123/artifacts?per_page=100']['artifacts'][-1]['created_at'] = '2026-10-10T08:01:00Z'
                else:
                    self.responses['releases/latest']['assets'][0]['digest'] = 'sha256:' + '3' * 64
                with self.assertRaises(ValueError):
                    self.check()
