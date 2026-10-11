#!/usr/bin/env python3
"""Prepare reviewed release trust from a genuine completed native certificate.

Read-only against GitHub. The output belongs in a reviewed configuration PR;
this command never enables workflows remotely or fabricates acceptance evidence.
"""
import argparse
import hashlib
import io
import json
import os
import subprocess
import zipfile
from datetime import datetime
from pathlib import Path

from issue_release_certificate import certification_profiles
from publish_certified_release import GitHub, expected_coverage
from release_certificate import authenticate, require, trusted_file_paths


def expected_cells(profiles):
    return expected_coverage(profiles, '')['expected_cells']


def read_certificate(data):
    require(len(data) <= 5 * 1024 * 1024, 'Certificate artifact exceeds limit')
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        require(archive.namelist() == ['certificate.json'], 'Unexpected certificate artifact contents')
        entry = archive.getinfo('certificate.json')
        require(entry.file_size <= 1024 * 1024, 'Certificate exceeds limit')
        require((entry.external_attr >> 16) & 0o170000 != 0o120000, 'Symlink certificate')
        return json.loads(archive.read(entry))


def bootstrap(client, run_id, policy_bytes, ledger, latest, reviewed_blobs):
    policy = json.loads(policy_bytes)
    profiles = certification_profiles(latest, ledger, policy['maxHistoricalProfiles'])
    repository = client.get('')
    require(repository['full_name'] == client.repository, 'Repository substitution')
    workflow = client.get('actions/workflows/release-certification.yml')
    run = client.get(f'actions/runs/{run_id}')
    require(run['event'] == 'workflow_dispatch' and run['status'] == 'completed'
            and run['conclusion'] == 'success', 'Genuine successful dispatch required')
    require(run['workflow_id'] == workflow['id']
            and workflow['path'] == '.github/workflows/release-certification.yml', 'Untrusted workflow')
    require(run['head_branch'] == repository['default_branch'], 'Default-branch certification required')
    require(set(reviewed_blobs) == set(trusted_file_paths), 'Incomplete locally reviewed execution surface')
    remote_blobs = {path: client.get(f'contents/{path}?ref={run["head_sha"]}')['sha']
                    for path in trusted_file_paths}
    require(remote_blobs == reviewed_blobs, 'Review issuing executable files before bootstrapping trust')
    artifacts = client.get(f'actions/runs/{run_id}/artifacts?per_page=100')
    matches = [item for item in artifacts['artifacts'] if item['name'] == 'release-certificate'
               and not item['expired']]
    require(len(matches) == 1, 'Missing or ambiguous genuine certificate artifact')
    artifact = matches[0]
    require(artifact.get('workflow_run', {}).get('id') == run_id
            and artifact['workflow_run'].get('head_sha') == run['head_sha'], 'Foreign certificate artifact')
    created, started, completed = (datetime.fromisoformat(value.replace('Z', '+00:00'))
                                   for value in (artifact['created_at'], run['run_started_at'], run['updated_at']))
    require(started <= created <= completed, 'Certificate belongs to another run attempt')
    data = client.get(f'actions/artifacts/{artifact["id"]}/zip', raw=True)
    require(artifact.get('digest') == 'sha256:' + hashlib.sha256(data).hexdigest(), 'Certificate byte substitution')
    certificate = read_certificate(data)
    require(certificate['authorization']['run_id'] == run_id, 'Certificate run substitution')
    trust = {'enabled': True, 'repository': client.repository, 'workflow_id': workflow['id'],
             'workflow_path': workflow['path'], 'default_branch': repository['default_branch'],
             'policy_hash': hashlib.sha256(policy_bytes).hexdigest(),
             'required_profile_fingerprints': [profile['fingerprint'] for profile in profiles],
             'expected_cells': expected_cells(profiles), 'reviewed_file_blobs': reviewed_blobs,
             'merge_staged_leaf': True, 'promote_images': True,
             'bootstrap_run_id': run_id, 'bootstrap_run_attempt': run['run_attempt']}
    source = client.get('commits/' + certificate['candidate_source_sha'])
    authenticate(certificate, trust, run, workflow, artifacts, source, remote_blobs)
    # Initial coverage proves the bootstrap, but must not freeze future Plex
    # inputs. Every later publisher independently resolves current latest and
    # historical profiles and requires fresh evidence for all of them.
    trust['require_current_upstream'] = True
    trust['bootstrap_profile_fingerprints'] = trust.pop('required_profile_fingerprints')
    trust['bootstrap_expected_cells'] = trust.pop('expected_cells')
    return trust


def local_blobs(root):
    # Git blobs describe reviewed committed bytes. Ignore uncommitted files.
    return {path: subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD:' + path],
                                         text=True).strip() for path in trusted_file_paths}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repository', required=True)
    parser.add_argument('--run-id', type=int, required=True)
    parser.add_argument('--latest-profile', required=True)
    parser.add_argument('--root', type=Path, default=Path('.'))
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    require(not args.output.exists(), 'Existing trust configuration must not be overwritten')
    trust = bootstrap(GitHub(args.repository, os.environ['GH_TOKEN']), args.run_id,
                      (args.root / '.github/plex-compatibility-policy.json').read_bytes(),
                      json.loads((args.root / '.github/plex-compatibility-profiles.json').read_text()),
                      json.loads(Path(args.latest_profile).read_text()), local_blobs(args.root))
    args.output.write_text(json.dumps(trust, indent=2) + '\n')
    print('Prepared genuine bootstrap configuration; review and commit it before enabling publication.')
