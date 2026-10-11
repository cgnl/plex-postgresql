#!/usr/bin/env python3
"""Fail-closed, dependency-free native release provenance checks."""
import argparse
import hashlib
import json
import re
import subprocess
from datetime import datetime
from pathlib import Path

trusted_file_paths = [
    '.github/workflows/docker-publish.yml', '.github/workflows/release-certification.yml',
    '.github/workflows/publish-certified-release.yml', '.github/workflows/stage-compatibility-release.yml',
    'scripts/release_certificate.py', 'scripts/issue_release_certificate.py',
    'scripts/publish_certified_release.py', 'scripts/promote_certified_images.py',
    'scripts/stage_compatibility_release.py', 'scripts/validate_certification_source.py',
    'scripts/verify_candidate_payload.py', 'scripts/native_compatibility_observation.py',
    'scripts/bootstrap_release_trust.py',
    'scripts/plex_compatibility.py', 'scripts/check-upstream-updates.py',
    'Dockerfile', 'Dockerfile.standalone',
    '.github/plex-compatibility-policy.json', '.github/plex-compatibility-profiles.json',
]
# Every tracked driver/helper can affect evidence or privileged execution. New
# helper files are listed above before their first commit; later script additions
# change this set and therefore require fresh reviewed trust bootstrap.
trusted_file_paths = sorted(set(trusted_file_paths) | set(subprocess.check_output(
    ['git', 'ls-files', 'scripts/'], cwd=Path(__file__).resolve().parents[1], text=True).splitlines()))
def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def profile_inputs(profile, profile_id):
    from plex_compatibility import validate_profile
    validate_profile(profile)
    require(profile.get('schema_version') == 1, 'Unsupported profile schema')
    require(re.fullmatch(r'[A-Za-z0-9-]+', profile_id or '') is not None, 'Invalid profile ID')
    require(profile.get('id') == profile_id, 'Profile ID mismatch')
    require(re.fullmatch(r'[0-9]+(?:\.[0-9]+){3}-[0-9a-f]+', profile.get('plex_version', '')) is not None, 'Invalid Plex version')
    for name in ('linuxserver', 'plexinc', 'builder', 'postgres', 'postgres15', 'postgres18'):
        image = profile['images'][name]
        require(re.fullmatch(r'[^\s@]+@sha256:[0-9a-f]{64}', image.get('ref', '')) is not None, 'Unpinned image')
        require(image['ref'].endswith('@' + image['digest']), 'Image digest mismatch')
        require(set(image['platforms']) == {'amd64', 'arm64'}, 'Missing native platform')
        require(all(re.fullmatch(r'sha256:[0-9a-f]{64}', value) for value in image['platforms'].values()), 'Invalid platform digest')
    require(re.fullmatch(r'[0-9a-f]{64}', profile.get('fingerprint', '')) is not None, 'Missing immutable fingerprint')
    return profile


def validate_workload(result, *, source, version, variant, arch, image, soak=False):
    expected = {'plex_version': version, 'variant': variant, 'arch': arch, 'candidate': image,
                'phase': 'native-workload-complete', 'exit_code': 0, 'workload_passed': True,
                'promotion_allowed': False, 'restart_cycles_requested': 1 if soak else 100,
                'restart_cycles_completed': 1 if soak else 100}
    for key in ('live_postgres_recovery_verified', 'decoded_fixture_verified',
                'real_movie_and_tv_playback_verified', 'real_media_full_decode_verified',
                'native_import_and_rollback_verified', 'four_concurrent_real_media_clients_verified'):
        expected[key] = True
    require(all(result.get(key) == value for key, value in expected.items()), 'Incomplete native workload')
    require(re.fullmatch(r'[0-9a-f]{40}', source) is not None, 'Invalid source SHA')
    if soak:
        require(result.get('soak_seconds_requested') == 21000 and result.get('soak_seconds_completed', 0) >= 21000,
                'Insufficient sustained workload')


def authenticate(certificate, trust, run, workflow, artifacts, source, reviewed_blobs=None, expected_coverage=None):
    """API responses are independently fetched by the privileged consumer."""
    require(trust.get('enabled') is True, 'Publication unconfigured: real bootstrap required')
    auth = certificate['authorization']
    require(certificate.get('schema_version') == 1 and certificate.get('mode') == 'full', 'Probe cannot authorize publication')
    require(certificate['candidate_source_sha'] == source['sha'], 'Candidate source changed')
    require(certificate['candidate_tree_sha'] == source['commit']['tree']['sha'], 'Candidate tree changed')
    require(auth['repository'] == trust['repository'] == run['repository']['full_name'], 'Foreign repository')
    require(auth['workflow_id'] == trust['workflow_id'] == run['workflow_id'] == workflow['id'], 'Untrusted workflow')
    require(workflow['path'] == trust['workflow_path'], 'Workflow path mismatch')
    if 'reviewed_file_blobs' in trust:
        require(set(trust['reviewed_file_blobs']) == set(trusted_file_paths), 'Incomplete trusted execution surface')
        require(reviewed_blobs == trust['reviewed_file_blobs'], 'Issuing workflow changed reviewed executable code')
    else:
        require(auth['workflow_revision'] == trust['workflow_revision'], 'Untrusted workflow revision')
    require(run['head_sha'] == auth['workflow_revision'] == auth['head_sha'], 'Workflow did not execute trusted revision')
    require(run['event'] == auth['event'] == 'workflow_dispatch', 'PR and other events cannot authorize release')
    require(run['id'] == auth['run_id'] and run['run_attempt'] == auth['run_attempt'], 'Run attempt substitution')
    require(run['status'] == 'completed' and run['conclusion'] == auth['conclusion'] == 'success', 'Issuing run incomplete')
    require(run['head_branch'] == trust['default_branch'], 'Issuing workflow is not on default branch')
    require(certificate['policy_hash'] == trust['policy_hash'], 'Policy changed')
    if trust.get('require_current_upstream') is True:
        require(expected_coverage is not None, 'Independent current-upstream coverage is required')
        require(expected_coverage['policy_hash'] == trust['policy_hash'], 'Current policy changed')
        fingerprints = expected_coverage['profile_fingerprints']
        expected_cells = expected_coverage['expected_cells']
    else:
        fingerprints = trust['required_profile_fingerprints']
        expected_cells = trust['expected_cells']
    require(set(certificate['profile_fingerprints']) == set(fingerprints), 'Profile coverage changed or upstream superseded')
    expected = set(certificate['expected_cells'])
    require(expected and expected == set(certificate['observed_cells']) and len(expected) == len(certificate['observed_cells']), 'Missing or duplicate cells')
    require(expected == set(expected_cells), 'Unexpected required cells')
    require(re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', certificate['project_version']) is not None, 'Not a regular version')
    api_artifacts = {item['id']: item for item in artifacts['artifacts']}
    require(certificate['artifacts'], 'No certified release bytes')
    for item in certificate['artifacts']:
        remote = api_artifacts.get(item['artifact_id'], {})
        require(remote.get('name') == item['name'] and remote.get('expired') is False, 'Artifact missing or expired')
        require(remote.get('workflow_run', {}).get('id') == run['id'], 'Foreign artifact owner')
        require(remote.get('workflow_run', {}).get('head_sha') == auth['workflow_revision'], 'Artifact workflow revision mismatch')
        created = datetime.fromisoformat(remote['created_at'].replace('Z', '+00:00'))
        started = datetime.fromisoformat(run['run_started_at'].replace('Z', '+00:00'))
        completed = datetime.fromisoformat(run['updated_at'].replace('Z', '+00:00'))
        require(started <= created <= completed, 'Artifact belongs to another run attempt')
        require(remote.get('digest') == 'sha256:' + item['sha256'], 'Artifact digest substitution')
        require(item['kind'] == 'linux-certified-bundle', 'Uncertified platform asset')
    require(certificate['tested_image_digests'] and all(re.fullmatch(r'ghcr\.io/[^\s@]+@sha256:[0-9a-f]{64}', x) for x in certificate['tested_image_digests']), 'Mutable image candidate')
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile')
    parser.add_argument('--profile-json')
    parser.add_argument('--profile-id')
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    require(bool(args.profile) != bool(args.profile_json), 'Supply exactly one profile transport')
    profile = json.loads(Path(args.profile).read_text() if args.profile else args.profile_json)
    profile_inputs(profile, args.profile_id)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(profile, indent=2) + '\n')
    import os
    if 'GITHUB_OUTPUT' in os.environ:
        with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
            stream.write('plex_version=' + profile['plex_version'] + '\n')
            for name, image in profile['images'].items():
                stream.write(name + '_image=' + image['ref'] + '\n')


if __name__ == '__main__':
    main()
